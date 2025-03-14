use iter_extended::vecmap;
use noirc_errors::Location;
use rustc_hash::FxHashSet as HashSet;

use crate::{
    DataType, Kind, Shared, Type, TypeAlias, TypeBindings,
    ast::{
        ERROR_IDENT, Expression, ExpressionKind, Ident, ItemVisibility, Path, Pattern, TypePath,
        UnresolvedType,
    },
    hir::{
        def_collector::dc_crate::CompilationError,
        resolution::errors::ResolverError,
        type_check::{Source, TypeCheckError},
    },
    hir_def::{
        expr::{HirExpression, HirIdent, HirMethodReference, ImplKind, TraitMethod},
        stmt::HirPattern,
    },
    node_interner::{
        DefinitionId, DefinitionInfo, DefinitionKind, ExprId, FuncId, GlobalId, TraitImplKind,
    },
};

use super::{Elaborator, ResolverMeta, path_resolution::PathResolutionItem};

impl Elaborator<'_> {
    pub(super) fn elaborate_pattern(
        &mut self,
        pattern: Pattern,
        expected_type: Type,
        definition_kind: DefinitionKind,
        warn_if_unused: bool,
    ) -> HirPattern {
        self.elaborate_pattern_mut(
            pattern,
            expected_type,
            definition_kind,
            None,
            &mut Vec::new(),
            warn_if_unused,
        )
    }

    /// Equivalent to `elaborate_pattern`, this version just also
    /// adds any new DefinitionIds that were created to the given Vec.
    pub fn elaborate_pattern_and_store_ids(
        &mut self,
        pattern: Pattern,
        expected_type: Type,
        definition_kind: DefinitionKind,
        created_ids: &mut Vec<HirIdent>,
        warn_if_unused: bool,
    ) -> HirPattern {
        self.elaborate_pattern_mut(
            pattern,
            expected_type,
            definition_kind,
            None,
            created_ids,
            warn_if_unused,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn elaborate_pattern_mut(
        &mut self,
        pattern: Pattern,
        expected_type: Type,
        definition: DefinitionKind,
        mutable: Option<Location>,
        new_definitions: &mut Vec<HirIdent>,
        warn_if_unused: bool,
    ) -> HirPattern {
        match pattern {
            Pattern::Identifier(name) => {
                // If this definition is mutable, do not store the rhs because it will
                // not always refer to the correct value of the variable
                let definition = match (mutable, definition) {
                    (Some(_), DefinitionKind::Local(_)) => DefinitionKind::Local(None),
                    (_, other) => other,
                };
                let ident = if let DefinitionKind::Global(global_id) = definition {
                    // Globals don't need to be added to scope, they're already in the def_maps
                    let id = self.interner.get_global(global_id).definition_id;
                    let location = name.location();
                    HirIdent::non_trait_method(id, location)
                } else {
                    self.add_variable_decl(
                        name,
                        mutable.is_some(),
                        true, // allow_shadowing
                        warn_if_unused,
                        definition,
                    )
                };
                self.interner.push_definition_type(ident.id, expected_type);
                new_definitions.push(ident.clone());
                HirPattern::Identifier(ident)
            }
            Pattern::Mutable(pattern, location, _) => {
                if let Some(first_mut) = mutable {
                    self.push_err(ResolverError::UnnecessaryMut {
                        first_mut,
                        second_mut: location,
                    });
                }

                let pattern = self.elaborate_pattern_mut(
                    *pattern,
                    expected_type,
                    definition,
                    Some(location),
                    new_definitions,
                    warn_if_unused,
                );
                HirPattern::Mutable(Box::new(pattern), location)
            }
            Pattern::Tuple(fields, location) => {
                let field_types = match expected_type.follow_bindings() {
                    Type::Tuple(fields) => fields,
                    Type::Error => Vec::new(),
                    expected_type => {
                        let tuple =
                            Type::Tuple(vecmap(&fields, |_| self.interner.next_type_variable()));

                        self.push_err(TypeCheckError::TypeMismatchWithSource {
                            expected: expected_type,
                            actual: tuple,
                            location,
                            source: Source::Assignment,
                        });
                        Vec::new()
                    }
                };

                let fields = vecmap(fields.into_iter().enumerate(), |(i, field)| {
                    let field_type = field_types.get(i).cloned().unwrap_or(Type::Error);
                    self.elaborate_pattern_mut(
                        field,
                        field_type,
                        definition.clone(),
                        mutable,
                        new_definitions,
                        warn_if_unused,
                    )
                });
                HirPattern::Tuple(fields, location)
            }
            Pattern::Struct(name, fields, location) => self.elaborate_struct_pattern(
                name,
                fields,
                location,
                expected_type,
                definition,
                mutable,
                new_definitions,
            ),
            Pattern::Interned(id, _) => {
                let pattern = self.interner.get_pattern(id).clone();
                self.elaborate_pattern_mut(
                    pattern,
                    expected_type,
                    definition,
                    mutable,
                    new_definitions,
                    warn_if_unused,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn elaborate_struct_pattern(
        &mut self,
        name: Path,
        fields: Vec<(Ident, Pattern)>,
        location: Location,
        expected_type: Type,
        definition: DefinitionKind,
        mutable: Option<Location>,
        new_definitions: &mut Vec<HirIdent>,
    ) -> HirPattern {
        let last_segment = name.last_segment();
        let name_location = last_segment.ident.location();
        let is_self_type = last_segment.ident.is_self_type_name();

        let error_identifier = |this: &mut Self| {
            // Must create a name here to return a HirPattern::Identifier. Allowing
            // shadowing here lets us avoid further errors if we define ERROR_IDENT
            // multiple times.
            let name = ERROR_IDENT.into();
            let identifier = this.add_variable_decl(name, false, true, true, definition.clone());
            HirPattern::Identifier(identifier)
        };

        let (struct_type, generics) = match self.lookup_type_or_error(name) {
            Some(Type::DataType(struct_type, struct_generics))
                if struct_type.borrow().is_struct() =>
            {
                (struct_type, struct_generics)
            }
            None => return error_identifier(self),
            Some(typ) => {
                let typ = typ.to_string();
                self.push_err(ResolverError::NonStructUsedInConstructor { typ, location });
                return error_identifier(self);
            }
        };

        let turbofish_location = last_segment.turbofish_location();

        let generics = self.resolve_struct_turbofish_generics(
            &struct_type.borrow(),
            generics,
            last_segment.generics,
            turbofish_location,
        );

        let actual_type = Type::DataType(struct_type.clone(), generics);

        self.unify(&actual_type, &expected_type, || TypeCheckError::TypeMismatchWithSource {
            expected: expected_type.clone(),
            actual: actual_type.clone(),
            location,
            source: Source::Assignment,
        });

        let typ = struct_type.clone();
        let fields = self.resolve_constructor_pattern_fields(
            typ,
            fields,
            location,
            expected_type.clone(),
            definition,
            mutable,
            new_definitions,
        );

        let struct_id = struct_type.borrow().id;

        self.interner.add_type_reference(struct_id, name_location, is_self_type);

        for (field_index, field) in fields.iter().enumerate() {
            let reference_location = field.0.location();
            self.interner.add_struct_member_reference(struct_id, field_index, reference_location);
        }

        HirPattern::Struct(expected_type, fields, location)
    }

    /// Resolve all the fields of a struct constructor expression.
    /// Ensures all fields are present, none are repeated, and all
    /// are part of the struct.
    #[allow(clippy::too_many_arguments)]
    fn resolve_constructor_pattern_fields(
        &mut self,
        struct_type: Shared<DataType>,
        fields: Vec<(Ident, Pattern)>,
        location: Location,
        expected_type: Type,
        definition: DefinitionKind,
        mutable: Option<Location>,
        new_definitions: &mut Vec<HirIdent>,
    ) -> Vec<(Ident, HirPattern)> {
        let mut ret = Vec::with_capacity(fields.len());
        let mut seen_fields = HashSet::default();
        let mut unseen_fields = struct_type
            .borrow()
            .field_names()
            .expect("This type should already be validated to be a struct");

        for (field, pattern) in fields {
            let (field_type, visibility) = expected_type
                .get_field_type_and_visibility(field.as_str())
                .unwrap_or((Type::Error, ItemVisibility::Public));
            let resolved = self.elaborate_pattern_mut(
                pattern,
                field_type,
                definition.clone(),
                mutable,
                new_definitions,
                true, // warn_if_unused
            );

            if unseen_fields.contains(&field) {
                unseen_fields.remove(&field);
                seen_fields.insert(field.clone());

                self.check_struct_field_visibility(
                    &struct_type.borrow(),
                    field.as_str(),
                    visibility,
                    field.location(),
                );
            } else if seen_fields.contains(&field) {
                // duplicate field
                self.push_err(ResolverError::DuplicateField { field: field.clone() });
            } else {
                // field not required by struct
                self.push_err(ResolverError::NoSuchField {
                    field: field.clone(),
                    struct_definition: struct_type.borrow().name.clone(),
                });
            }

            ret.push((field, resolved));
        }

        if !unseen_fields.is_empty() {
            self.push_err(ResolverError::MissingFields {
                location,
                missing_fields: unseen_fields.into_iter().map(|field| field.to_string()).collect(),
                struct_definition: struct_type.borrow().name.clone(),
            });
        }

        ret
    }

    pub(super) fn add_variable_decl(
        &mut self,
        name: Ident,
        mutable: bool,
        allow_shadowing: bool,
        warn_if_unused: bool,
        definition: DefinitionKind,
    ) -> HirIdent {
        if let DefinitionKind::Global(global_id) = definition {
            return self.add_global_variable_decl(name, global_id);
        }

        let location = name.location();
        let name = name.into_string();
        let comptime = self.in_comptime_context();
        let id =
            self.interner.push_definition(name.clone(), mutable, comptime, definition, location);
        let ident = HirIdent::non_trait_method(id, location);
        let resolver_meta =
            ResolverMeta { num_times_used: 0, ident: ident.clone(), warn_if_unused };

        if name != "_" {
            let scope = self.scopes.get_mut_scope();
            let old_value = scope.add_key_value(name.clone(), resolver_meta);

            if !allow_shadowing {
                if let Some(old_value) = old_value {
                    self.push_err(ResolverError::DuplicateDefinition {
                        name,
                        first_location: old_value.ident.location,
                        second_location: location,
                    });
                }
            }
        }

        ident
    }

    pub fn add_existing_variable_to_scope(
        &mut self,
        name: String,
        ident: HirIdent,
        warn_if_unused: bool,
    ) {
        let second_location = ident.location;
        let resolver_meta = ResolverMeta { num_times_used: 0, ident, warn_if_unused };

        let old_value = self.scopes.get_mut_scope().add_key_value(name.clone(), resolver_meta);

        if let Some(old_value) = old_value {
            let first_location = old_value.ident.location;
            self.push_err(ResolverError::DuplicateDefinition {
                name,
                first_location,
                second_location,
            });
        }
    }

    pub fn add_global_variable_decl(&mut self, name: Ident, global_id: GlobalId) -> HirIdent {
        let scope = self.scopes.get_mut_scope();
        let global = self.interner.get_global(global_id);
        let ident = HirIdent::non_trait_method(global.definition_id, global.location);
        let resolver_meta =
            ResolverMeta { num_times_used: 0, ident: ident.clone(), warn_if_unused: true };

        let old_global_value = scope.add_key_value(name.to_string(), resolver_meta);
        if let Some(old_global_value) = old_global_value {
            self.push_err(ResolverError::DuplicateDefinition {
                first_location: old_global_value.ident.location,
                second_location: name.location(),
                name: name.into_string(),
            });
        }
        ident
    }

    /// Lookup and use the specified variable.
    /// This will increment its use counter by one and return the variable if found.
    /// If the variable is not found, an error is returned.
    pub(super) fn use_variable(
        &mut self,
        name: &Ident,
    ) -> Result<(HirIdent, usize), ResolverError> {
        // Find the definition for this Ident
        let scope_tree = self.scopes.current_scope_tree();
        let variable = scope_tree.find(name.as_str());

        let location = name.location();
        if let Some((variable_found, scope)) = variable {
            variable_found.num_times_used += 1;
            let id = variable_found.ident.id;
            Ok((HirIdent::non_trait_method(id, location), scope))
        } else {
            Err(ResolverError::VariableNotDeclared {
                name: name.to_string(),
                location: name.location(),
            })
        }
    }

    /// Resolve generics using the expected kinds of the function we are calling
    pub(super) fn resolve_function_turbofish_generics(
        &mut self,
        func_id: &FuncId,
        unresolved_turbofish: Option<Vec<UnresolvedType>>,
        location: Location,
    ) -> Option<Vec<Type>> {
        let direct_generic_kinds =
            vecmap(&self.interner.function_meta(func_id).direct_generics, |generic| generic.kind());

        unresolved_turbofish.map(|unresolved_turbofish| {
            if unresolved_turbofish.len() != direct_generic_kinds.len() {
                let type_check_err = TypeCheckError::IncorrectTurbofishGenericCount {
                    expected_count: direct_generic_kinds.len(),
                    actual_count: unresolved_turbofish.len(),
                    location,
                };
                self.push_err(type_check_err);
            }

            self.resolve_turbofish_generics(direct_generic_kinds, unresolved_turbofish)
        })
    }

    pub(super) fn resolve_struct_turbofish_generics(
        &mut self,
        struct_type: &DataType,
        generics: Vec<Type>,
        unresolved_turbofish: Option<Vec<UnresolvedType>>,
        location: Location,
    ) -> Vec<Type> {
        let kinds = vecmap(&struct_type.generics, |generic| generic.kind());
        self.resolve_item_turbofish_generics(
            "struct",
            struct_type.name.as_str(),
            kinds,
            generics,
            unresolved_turbofish,
            location,
        )
    }

    pub(super) fn resolve_trait_turbofish_generics(
        &mut self,
        trait_name: &str,
        trait_generic_kinds: Vec<Kind>,
        generics: Vec<Type>,
        unresolved_turbofish: Option<Vec<UnresolvedType>>,
        location: Location,
    ) -> Vec<Type> {
        self.resolve_item_turbofish_generics(
            "trait",
            trait_name,
            trait_generic_kinds,
            generics,
            unresolved_turbofish,
            location,
        )
    }

    pub(super) fn resolve_alias_turbofish_generics(
        &mut self,
        type_alias: &TypeAlias,
        generics: Vec<Type>,
        unresolved_turbofish: Option<Vec<UnresolvedType>>,
        location: Location,
    ) -> Vec<Type> {
        let kinds = vecmap(&type_alias.generics, |generic| generic.kind());
        self.resolve_item_turbofish_generics(
            "alias",
            type_alias.name.as_str(),
            kinds,
            generics,
            unresolved_turbofish,
            location,
        )
    }

    pub(super) fn resolve_item_turbofish_generics(
        &mut self,
        item_kind: &'static str,
        item_name: &str,
        item_generic_kinds: Vec<Kind>,
        generics: Vec<Type>,
        unresolved_turbofish: Option<Vec<UnresolvedType>>,
        location: Location,
    ) -> Vec<Type> {
        let Some(turbofish_generics) = unresolved_turbofish else {
            return generics;
        };

        if turbofish_generics.len() != generics.len() {
            self.push_err(TypeCheckError::GenericCountMismatch {
                item: format!("{item_kind} {item_name}"),
                expected: generics.len(),
                found: turbofish_generics.len(),
                location,
            });
            return generics;
        }

        self.resolve_turbofish_generics(item_generic_kinds, turbofish_generics)
    }

    pub(super) fn resolve_turbofish_generics(
        &mut self,
        kinds: Vec<Kind>,
        turbofish_generics: Vec<UnresolvedType>,
    ) -> Vec<Type> {
        let kinds_with_types = kinds.into_iter().zip(turbofish_generics);
        vecmap(kinds_with_types, |(kind, unresolved_type)| {
            self.resolve_type_inner(unresolved_type, &kind)
        })
    }

    pub(super) fn elaborate_variable(&mut self, variable: Path) -> (ExprId, Type) {
        let unresolved_turbofish = variable.segments.last().unwrap().generics.clone();

        let location = variable.location;
        let (expr, item) = self.resolve_variable(variable);
        let definition_id = expr.id;

        let type_generics = item.map(|item| self.resolve_item_turbofish(item)).unwrap_or_default();

        let definition = self.interner.try_definition(definition_id);
        let is_comptime_local = !self.in_comptime_context()
            && definition.is_some_and(DefinitionInfo::is_comptime_local);
        let definition_kind = definition.as_ref().map(|definition| definition.kind.clone());

        let mut bindings = TypeBindings::new();

        // Resolve any generics if we the variable we have resolved is a function
        // and if the turbofish operator was used.
        let generics = if let Some(DefinitionKind::Function(func_id)) = &definition_kind {
            self.resolve_function_turbofish_generics(func_id, unresolved_turbofish, location)
        } else {
            None
        };

        // If this is a function call on a type that has generics, we need to bind those generic types.
        if !type_generics.is_empty() {
            if let Some(DefinitionKind::Function(func_id)) = &definition_kind {
                // `all_generics` will always have the enclosing type generics first, so we need to bind those
                let func_generics = &self.interner.function_meta(func_id).all_generics;
                for (type_generic, func_generic) in type_generics.into_iter().zip(func_generics) {
                    let type_var = &func_generic.type_var;
                    bindings
                        .insert(type_var.id(), (type_var.clone(), type_var.kind(), type_generic));
                }
            }
        }

        let id = self.interner.push_expr(HirExpression::Ident(expr.clone(), generics.clone()));

        self.interner.push_expr_location(id, location);
        let typ = self.type_check_variable_with_bindings(expr, id, generics, bindings);
        self.interner.push_expr_type(id, typ.clone());

        // If this variable it a comptime local variable, use its current value as the final expression
        if is_comptime_local {
            let mut interpreter = self.setup_interpreter();
            let value = interpreter.evaluate(id);
            // If the value is an error it means the variable already had an error, so don't report it here again
            // (the error will make no sense, it will say that a non-comptime variable was referenced at runtime
            // but that's not true)
            if value.is_ok() {
                let (id, typ) = self.inline_comptime_value(value, location);
                self.debug_comptime(location, |interner| id.to_display_ast(interner).kind);
                (id, typ)
            } else {
                (id, typ)
            }
        } else {
            (id, typ)
        }
    }

    /// Solve any generics that are part of the path before the function, for example:
    ///
    /// ```rust
    /// foo::Bar::<i32>::baz
    ///           ^^^^^
    ///         solve these
    /// ```
    fn resolve_item_turbofish(&mut self, item: PathResolutionItem) -> Vec<Type> {
        match item {
            PathResolutionItem::Method(struct_id, Some(generics), _func_id) => {
                let struct_type = self.interner.get_type(struct_id);
                let struct_type = struct_type.borrow();
                let struct_generics = struct_type.instantiate(self.interner);
                self.resolve_struct_turbofish_generics(
                    &struct_type,
                    struct_generics,
                    Some(generics.generics),
                    generics.location,
                )
            }
            PathResolutionItem::TypeAliasFunction(type_alias_id, generics, _func_id) => {
                let type_alias = self.interner.get_type_alias(type_alias_id);
                let type_alias = type_alias.borrow();
                let alias_generics = vecmap(&type_alias.generics, |generic| {
                    self.interner.next_type_variable_with_kind(generic.kind())
                });

                // First solve the generics on the alias, if any
                let generics = if let Some(generics) = generics {
                    self.resolve_alias_turbofish_generics(
                        &type_alias,
                        alias_generics,
                        Some(generics.generics),
                        generics.location,
                    )
                } else {
                    alias_generics
                };

                // Now instantiate the underlying struct or alias with those generics, the struct might
                // have more generics than those in the alias, like in this example:
                //
                // type Alias<T> = Struct<T, i32>;
                get_type_alias_generics(&type_alias, &generics)
            }
            PathResolutionItem::TraitFunction(trait_id, Some(generics), _func_id) => {
                let trait_ = self.interner.get_trait(trait_id);
                let kinds = vecmap(&trait_.generics, |generic| generic.kind());
                let trait_generics =
                    vecmap(&kinds, |kind| self.interner.next_type_variable_with_kind(kind.clone()));

                self.resolve_trait_turbofish_generics(
                    &trait_.name.to_string(),
                    kinds,
                    trait_generics,
                    Some(generics.generics),
                    generics.location,
                )
            }
            _ => Vec::new(),
        }
    }

    fn resolve_variable(&mut self, path: Path) -> (HirIdent, Option<PathResolutionItem>) {
        if let Some(trait_path_resolution) = self.resolve_trait_generic_path(&path) {
            for error in trait_path_resolution.errors {
                self.push_err(error);
            }

            (
                HirIdent {
                    location: path.location,
                    id: self.interner.trait_method_id(trait_path_resolution.method.method_id),
                    impl_kind: ImplKind::TraitMethod(trait_path_resolution.method),
                },
                trait_path_resolution.item,
            )
        } else {
            // If the Path is being used as an Expression, then it is referring to a global from a separate module
            // Otherwise, then it is referring to an Identifier
            // This lookup allows support of such statements: let x = foo::bar::SOME_GLOBAL + 10;
            // If the expression is a singular indent, we search the resolver's current scope as normal.
            let location = path.location;
            let ((hir_ident, var_scope_index), item) = self.get_ident_from_path(path);

            if hir_ident.id != DefinitionId::dummy_id() {
                match self.interner.definition(hir_ident.id).kind {
                    DefinitionKind::Function(func_id) => {
                        if let Some(current_item) = self.current_item {
                            self.interner.add_function_dependency(current_item, func_id);
                        }

                        self.interner.add_function_reference(func_id, hir_ident.location);
                    }
                    DefinitionKind::Global(global_id) => {
                        self.elaborate_global_if_unresolved(&global_id);
                        if let Some(current_item) = self.current_item {
                            self.interner.add_global_dependency(current_item, global_id);
                        }

                        self.interner.add_global_reference(global_id, hir_ident.location);
                    }
                    DefinitionKind::NumericGeneric(_, ref numeric_typ) => {
                        // Initialize numeric generics to a polymorphic integer type in case
                        // they're used in expressions. We must do this here since type_check_variable
                        // does not check definition kinds and otherwise expects parameters to
                        // already be typed.
                        if self.interner.definition_type(hir_ident.id) == Type::Error {
                            let type_var_kind = Kind::Numeric(numeric_typ.clone());
                            let typ = self.type_variable_with_kind(type_var_kind);
                            self.interner.push_definition_type(hir_ident.id, typ);
                        }
                    }
                    DefinitionKind::Local(_) => {
                        // only local variables can be captured by closures.
                        self.resolve_local_variable(hir_ident.clone(), var_scope_index);

                        self.interner.add_local_reference(hir_ident.id, location);
                    }
                }
            }

            (hir_ident, item)
        }
    }

    pub(crate) fn type_check_variable(
        &mut self,
        ident: HirIdent,
        expr_id: ExprId,
        generics: Option<Vec<Type>>,
    ) -> Type {
        let bindings = TypeBindings::new();
        self.type_check_variable_with_bindings(ident, expr_id, generics, bindings)
    }

    pub(super) fn type_check_variable_with_bindings(
        &mut self,
        ident: HirIdent,
        expr_id: ExprId,
        generics: Option<Vec<Type>>,
        mut bindings: TypeBindings,
    ) -> Type {
        // Add type bindings from any constraints that were used.
        // We need to do this first since otherwise instantiating the type below
        // will replace each trait generic with a fresh type variable, rather than
        // the type used in the trait constraint (if it exists). See #4088.
        if let ImplKind::TraitMethod(method) = &ident.impl_kind {
            self.bind_generics_from_trait_constraint(
                &method.constraint,
                method.assumed,
                &mut bindings,
            );
        }

        // An identifiers type may be forall-quantified in the case of generic functions.
        // E.g. `fn foo<T>(t: T, field: Field) -> T` has type `forall T. fn(T, Field) -> T`.
        // We must instantiate identifiers at every call site to replace this T with a new type
        // variable to handle generic functions.
        let t = self.interner.id_type_substitute_trait_as_type(ident.id);

        let definition = self.interner.try_definition(ident.id);
        let function_generic_count = definition.map_or(0, |definition| match &definition.kind {
            DefinitionKind::Function(function) => {
                self.interner.function_modifiers(function).generic_count
            }
            _ => 0,
        });

        let location = self.interner.expr_location(&expr_id);

        // This instantiates a trait's generics as well which need to be set
        // when the constraint below is later solved for when the function is
        // finished. How to link the two?
        let (typ, bindings) =
            self.instantiate(t, bindings, generics, function_generic_count, location);

        // Push any trait constraints required by this definition to the context
        // to be checked later when the type of this variable is further constrained.
        if let Some(definition) = self.interner.try_definition(ident.id) {
            if let DefinitionKind::Function(function) = definition.kind {
                let function = self.interner.function_meta(&function);
                for mut constraint in function.trait_constraints.clone() {
                    constraint.apply_bindings(&bindings);

                    self.push_trait_constraint(
                        constraint, expr_id,
                        false, // This constraint shouldn't lead to choosing a trait impl method
                    );
                }
            }
        }

        if let ImplKind::TraitMethod(mut method) = ident.impl_kind {
            method.constraint.apply_bindings(&bindings);
            if method.assumed {
                let trait_generics = method.constraint.trait_bound.trait_generics.clone();
                let object_type = method.constraint.typ;
                let trait_impl = TraitImplKind::Assumed { object_type, trait_generics };
                self.interner.select_impl_for_expression(expr_id, trait_impl);
            } else {
                // Currently only one impl can be selected per expr_id, so this
                // constraint needs to be pushed after any other constraints so
                // that monomorphization can resolve this trait method to the correct impl.
                self.push_trait_constraint(
                    method.constraint,
                    expr_id,
                    true, // this constraint should lead to choosing a trait impl method
                );
            }
        }

        self.interner.store_instantiation_bindings(expr_id, bindings);
        typ
    }

    fn instantiate(
        &mut self,
        typ: Type,
        bindings: TypeBindings,
        turbofish_generics: Option<Vec<Type>>,
        function_generic_count: usize,
        location: Location,
    ) -> (Type, TypeBindings) {
        match turbofish_generics {
            Some(turbofish_generics) => {
                if turbofish_generics.len() != function_generic_count {
                    let type_check_err = TypeCheckError::IncorrectTurbofishGenericCount {
                        expected_count: function_generic_count,
                        actual_count: turbofish_generics.len(),
                        location,
                    };
                    self.push_err(CompilationError::TypeError(type_check_err));
                    typ.instantiate_with_bindings(bindings, self.interner)
                } else {
                    // Fetch the count of any implicit generics on the function, such as
                    // for a method within a generic impl.
                    let implicit_generic_count = match &typ {
                        Type::Forall(generics, _) => generics.len() - function_generic_count,
                        _ => 0,
                    };
                    typ.instantiate_with(turbofish_generics, self.interner, implicit_generic_count)
                }
            }
            None => typ.instantiate_with_bindings(bindings, self.interner),
        }
    }

    pub fn get_ident_from_path(
        &mut self,
        path: Path,
    ) -> ((HirIdent, usize), Option<PathResolutionItem>) {
        let location = Location::new(path.last_ident().span(), path.location.file);

        let error = match path.as_ident().map(|ident| self.use_variable(ident)) {
            Some(Ok(found)) => return (found, None),
            // Try to look it up as a global, but still issue the first error if we fail
            Some(Err(error)) => match self.lookup_global(path) {
                Ok((id, item)) => {
                    return ((HirIdent::non_trait_method(id, location), 0), Some(item));
                }
                Err(_) => error,
            },
            None => match self.lookup_global(path) {
                Ok((id, item)) => {
                    return ((HirIdent::non_trait_method(id, location), 0), Some(item));
                }
                Err(error) => error,
            },
        };
        self.push_err(error);
        let id = DefinitionId::dummy_id();
        ((HirIdent::non_trait_method(id, location), 0), None)
    }

    pub(super) fn elaborate_type_path(&mut self, path: TypePath) -> (ExprId, Type) {
        let location = path.item.location();
        let typ = self.resolve_type(path.typ);
        let check_self_param = false;

        let Some(method) = self.lookup_method(&typ, path.item.as_str(), location, check_self_param)
        else {
            let error = Expression::new(ExpressionKind::Error, location);
            return self.elaborate_expression(error);
        };

        let func_id = method
            .func_id(self.interner)
            .expect("Expected trait function to be a DefinitionKind::Function");

        let generics =
            path.turbofish.map(|turbofish| self.resolve_type_args(turbofish, func_id, location).0);

        let id = self.interner.function_definition_id(func_id);

        let impl_kind = match method {
            HirMethodReference::FuncId(_) => ImplKind::NotATraitMethod,
            HirMethodReference::TraitMethodId(method_id, generics, _) => {
                let mut constraint =
                    self.interner.get_trait(method_id.trait_id).as_constraint(location);
                constraint.trait_bound.trait_generics = generics;
                ImplKind::TraitMethod(TraitMethod { method_id, constraint, assumed: false })
            }
        };

        let ident = HirIdent { location, id, impl_kind };
        let id = self.interner.push_expr(HirExpression::Ident(ident.clone(), generics.clone()));
        self.interner.push_expr_location(id, location);

        let typ = self.type_check_variable(ident, id, generics);
        self.interner.push_expr_type(id, typ.clone());

        (id, typ)
    }
}

fn get_type_alias_generics(type_alias: &TypeAlias, generics: &[Type]) -> Vec<Type> {
    let typ = type_alias.get_type(generics);
    match typ {
        Type::DataType(_, generics) => generics,
        Type::Alias(type_alias, generics) => {
            get_type_alias_generics(&type_alias.borrow(), &generics)
        }
        _ => panic!("Expected type alias to point to struct or alias"),
    }
}

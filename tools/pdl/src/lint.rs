use std::collections::HashMap;

use crate::{ast::*, parser};

/// Gather information about the full AST.
#[derive(Debug)]
pub struct Scope<'d> {
    // Original file.
    file: &'d parser::ast::File,

    // Collection of Group, Packet, Enum, Struct, Checksum, and CustomField declarations.
    pub typedef: HashMap<String, &'d parser::ast::Decl>,

    // Collection of Packet, Struct, and Group scope declarations.
    pub scopes: HashMap<&'d parser::ast::Decl, PacketScope<'d>>,
}

/// Gather information about a Packet, Struct, or Group declaration.
#[derive(Debug)]
pub struct PacketScope<'d> {
    // Typedef, scalar, array fields.
    pub named: HashMap<String, &'d parser::ast::Field>,

    // Flattened field declarations.
    // Contains field declarations from the original Packet, Struct, or Group,
    // where Group fields have been substituted by their body.
    pub fields: Vec<&'d parser::ast::Field>,

    // Constraint declarations gathered from Group inlining.
    pub constraints: HashMap<String, &'d Constraint>,

    // Local and inherited field declarations. Only named fields are preserved.
    // Saved here for reference for parent constraint resolving.
    pub all_fields: HashMap<String, &'d parser::ast::Field>,

    // Local and inherited constraint declarations.
    // Saved here for constraint conflict checks.
    pub all_constraints: HashMap<String, &'d Constraint>,
}

impl<'d> std::hash::Hash for &'d parser::ast::Decl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(*self, state);
    }
}

impl<'d> PacketScope<'d> {
    /// Insert a field declaration into a packet scope.
    fn insert(&mut self, field: &'d parser::ast::Field) {
        field.id().and_then(|id| self.named.insert(id.to_owned(), field));
    }

    /// Add parent fields and constraints to the scope.
    /// Only named fields are imported.
    fn inherit(
        &mut self,
        parent: &PacketScope<'d>,
        constraints: impl Iterator<Item = &'d Constraint>,
    ) {
        // Check constraints.
        assert!(self.all_constraints.is_empty());
        self.all_constraints = parent.all_constraints.clone();
        for constraint in constraints {
            let id = constraint.id.clone();
            self.all_constraints.insert(id, constraint);
        }

        // Merge group constraints into parent constraints,
        // but generate no duplication warnings, the constraints
        // do no apply to the same field set.
        for (id, constraint) in self.constraints.iter() {
            self.all_constraints.insert(id.clone(), constraint);
        }

        // Save parent fields.
        self.all_fields = parent.all_fields.clone();
    }

    /// Insert group field declarations into a packet scope.
    fn inline(
        &mut self,
        packet_scope: &PacketScope<'d>,
        constraints: impl Iterator<Item = &'d Constraint>,
    ) {
        for (id, field) in packet_scope.named.iter() {
            self.named.insert(id.clone(), field);
        }

        // Append group fields to the finalizeed fields.
        for field in packet_scope.fields.iter() {
            self.fields.push(field);
        }

        // Append group constraints to the caller packet_scope.
        for (id, constraint) in packet_scope.constraints.iter() {
            self.constraints.insert(id.clone(), constraint);
        }

        // Add constraints to the packet_scope, checking for duplicate constraints.
        for constraint in constraints {
            let id = constraint.id.clone();
            self.constraints.insert(id, constraint);
        }
    }

    /// Lookup a field by name. This will also find the special
    /// `_payload_` and `_body_` fields.
    pub fn get_packet_field(&self, id: &str) -> Option<&parser::ast::Field> {
        self.named.get(id).copied().or(match id {
            "_payload_" | "_body_" => self.get_payload_field(),
            _ => None,
        })
    }

    /// Find the payload or body field, if any.
    pub fn get_payload_field(&self) -> Option<&parser::ast::Field> {
        self.fields
            .iter()
            .find(|field| matches!(&field.desc, FieldDesc::Payload { .. } | FieldDesc::Body { .. }))
            .copied()
    }

    /// Lookup the size field for an array field.
    pub fn get_array_size_field(&self, id: &str) -> Option<&parser::ast::Field> {
        self.fields
            .iter()
            .find(|field| match &field.desc {
                FieldDesc::Size { field_id, .. } | FieldDesc::Count { field_id, .. } => {
                    field_id == id
                }
                _ => false,
            })
            .copied()
    }

    /// Find the size field corresponding to the payload or body
    /// field of this packet.
    pub fn get_payload_size_field(&self) -> Option<&parser::ast::Field> {
        self.fields
            .iter()
            .find(|field| match &field.desc {
                FieldDesc::Size { field_id, .. } => field_id == "_payload_" || field_id == "_body_",
                _ => false,
            })
            .copied()
    }

    /// Cleanup scope after processing all fields.
    fn finalize(&mut self) {
        // Check field shadowing.
        for f in self.fields.iter() {
            if let Some(id) = f.id() {
                self.all_fields.insert(id.to_string(), f);
            }
        }
    }
}

impl<'d> Scope<'d> {
    pub fn new(file: &parser::ast::File) -> Scope<'_> {
        let mut scope = Scope { file, typedef: HashMap::new(), scopes: HashMap::new() };

        // Gather top-level declarations.
        // Validate the top-level scopes (Group, Packet, Typedef).
        //
        // TODO: switch to try_insert when stable
        for decl in &file.declarations {
            if let Some(id) = decl.id() {
                scope.typedef.insert(id.to_string(), decl);
            }
            if let Some(lscope) = decl_scope(decl) {
                scope.scopes.insert(decl, lscope);
            }
        }

        scope.finalize();
        scope
    }

    // Sort Packet, Struct, and Group declarations by reverse topological
    // orde, and inline Group fields.
    // Raises errors and warnings for:
    //      - undeclared included Groups,
    //      - undeclared Typedef fields,
    //      - undeclared Packet or Struct parents,
    //      - recursive Group insertion,
    //      - recursive Packet or Struct inheritance.
    fn finalize(&mut self) -> Vec<&'d parser::ast::Decl> {
        // Auxiliary function implementing BFS on Packet tree.
        enum Mark {
            Temporary,
            Permanent,
        }
        struct Context<'d> {
            list: Vec<&'d parser::ast::Decl>,
            visited: HashMap<&'d parser::ast::Decl, Mark>,
            scopes: HashMap<&'d parser::ast::Decl, PacketScope<'d>>,
        }

        fn bfs<'s, 'd>(
            decl: &'d parser::ast::Decl,
            context: &'s mut Context<'d>,
            scope: &Scope<'d>,
        ) -> Option<&'s PacketScope<'d>> {
            match context.visited.get(&decl) {
                Some(Mark::Permanent) => return context.scopes.get(&decl),
                Some(Mark::Temporary) => {
                    return None;
                }
                _ => (),
            }

            let (parent_id, fields) = match &decl.desc {
                DeclDesc::Packet { parent_id, fields, .. }
                | DeclDesc::Struct { parent_id, fields, .. } => (parent_id.as_ref(), fields),
                DeclDesc::Group { fields, .. } => (None, fields),
                _ => return None,
            };

            context.visited.insert(decl, Mark::Temporary);
            let mut lscope = decl_scope(decl).unwrap();

            // Iterate over Struct and Group fields.
            for f in fields {
                match &f.desc {
                    FieldDesc::Group { group_id, constraints, .. } => {
                        match scope.typedef.get(group_id) {
                            Some(group_decl @ Decl { desc: DeclDesc::Group { .. }, .. }) => {
                                // Recurse to flatten the inserted group.
                                if let Some(rscope) = bfs(group_decl, context, scope) {
                                    // Inline the group fields and constraints into
                                    // the current scope.
                                    lscope.inline(rscope, constraints.iter())
                                }
                            }
                            None | Some(_) => (),
                        }
                    }
                    FieldDesc::Typedef { type_id, .. } => {
                        lscope.fields.push(f);
                        match scope.typedef.get(type_id) {
                            Some(struct_decl @ Decl { desc: DeclDesc::Struct { .. }, .. }) => {
                                bfs(struct_decl, context, scope);
                            }
                            None | Some(_) => (),
                        }
                    }
                    _ => lscope.fields.push(f),
                }
            }

            // Iterate over parent declaration.
            let parent = parent_id.and_then(|id| scope.typedef.get(id));
            if let Some(parent_decl) = parent {
                if let Some(rscope) = bfs(parent_decl, context, scope) {
                    // Import the parent fields and constraints into the current scope.
                    lscope.inherit(rscope, decl.constraints())
                }
            }

            lscope.finalize();
            context.list.push(decl);
            context.visited.insert(decl, Mark::Permanent);
            context.scopes.insert(decl, lscope);
            context.scopes.get(&decl)
        }

        let mut context =
            Context::<'d> { list: vec![], visited: HashMap::new(), scopes: HashMap::new() };

        for decl in self.typedef.values() {
            bfs(decl, &mut context, self);
        }

        self.scopes = context.scopes;
        context.list
    }

    pub fn iter_children<'a>(
        &'a self,
        id: &'a str,
    ) -> impl Iterator<Item = &'d parser::ast::Decl> + 'a {
        self.file.iter_children(self.typedef.get(id).unwrap())
    }

    pub fn has_children(&self, id: &str) -> bool {
        self.iter_children(id).next().is_some()
    }
}

fn decl_scope(decl: &parser::ast::Decl) -> Option<PacketScope<'_>> {
    match &decl.desc {
        DeclDesc::Packet { fields, .. }
        | DeclDesc::Struct { fields, .. }
        | DeclDesc::Group { fields, .. } => {
            let mut scope = PacketScope {
                named: HashMap::new(),
                fields: Vec::new(),
                constraints: HashMap::new(),
                all_fields: HashMap::new(),
                all_constraints: HashMap::new(),
            };
            for field in fields {
                scope.insert(field)
            }
            Some(scope)
        }
        _ => None,
    }
}

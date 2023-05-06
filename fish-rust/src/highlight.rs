use crate::ast::{
    self, Argument, Ast, BlockStatement, BlockStatementHeaderVariant, DecoratedStatement, Keyword,
    Node, NodeFfi, NodeVisitor, Redirection, Token, Type, VariableAssignment,
};
use crate::expand::{expand_to_command_and_args, ExpandResultCode};
use crate::ffi::highlighter_t;
use crate::operation_context::OperationContext;
use crate::parse_constants::ParseTokenType;
use crate::wchar::{wstr, WString};
use crate::wchar_ffi::{AsWstr, WCharFromFFI, WCharToFFI};
use cxx::{CxxWString, UniquePtr};
use std::pin::Pin;

struct Highlighter<'a> {
    companion: Pin<&'a mut highlighter_t>,
    ast: &'a Ast,
}
impl<'a> Highlighter<'a> {
    // Visit the children of a node.
    fn visit_children(&mut self, node: &'a dyn Node) {
        node.accept(self, false);
    }
    // AST visitor implementations.
    fn visit_keyword(&mut self, node: &dyn Keyword) {
        let ffi_node = NodeFfi::new(node.leaf_as_node_ffi());
        self.companion
            .as_mut()
            .visit_keyword((&ffi_node as *const NodeFfi<'_>).cast());
    }
    fn visit_token(&mut self, node: &dyn Token) {
        let ffi_node = NodeFfi::new(node.leaf_as_node_ffi());
        self.companion
            .as_mut()
            .visit_token((&ffi_node as *const NodeFfi<'_>).cast());
    }
    fn visit_argument(&mut self, node: &Argument) {
        self.companion
            .as_mut()
            .visit_argument((node as *const Argument).cast(), false, true);
    }
    fn visit_redirection(&mut self, node: &Redirection) {
        self.companion
            .as_mut()
            .visit_redirection((node as *const Redirection).cast());
    }
    fn visit_variable_assignment(&mut self, node: &VariableAssignment) {
        self.companion
            .as_mut()
            .visit_variable_assignment((node as *const VariableAssignment).cast());
    }
    fn visit_semi_nl(&mut self, node: &dyn Node) {
        let ffi_node = NodeFfi::new(node);
        self.companion
            .as_mut()
            .visit_semi_nl((&ffi_node as *const NodeFfi<'_>).cast());
    }
    fn visit_decorated_statement(&mut self, node: &DecoratedStatement) {
        self.companion
            .as_mut()
            .visit_decorated_statement((node as *const DecoratedStatement).cast());
    }
    fn visit_block_statement(&mut self, node: &'a BlockStatement) {
        match &*node.header {
            BlockStatementHeaderVariant::None => panic!(),
            BlockStatementHeaderVariant::ForHeader(node) => self.visit(node),
            BlockStatementHeaderVariant::WhileHeader(node) => self.visit(node),
            BlockStatementHeaderVariant::FunctionHeader(node) => self.visit(node),
            BlockStatementHeaderVariant::BeginHeader(node) => self.visit(node),
        }
        self.visit(&node.args_or_redirs);
        let pending_variables_count = self
            .companion
            .as_mut()
            .visit_block_statement1((node as *const BlockStatement).cast());
        self.visit(&node.jobs);
        self.visit(&node.end);
        self.companion
            .as_mut()
            .visit_block_statement2(pending_variables_count);
    }
}

impl<'a> NodeVisitor<'a> for Highlighter<'a> {
    fn visit(&mut self, node: &'a dyn Node) {
        if let Some(keyword) = node.as_keyword() {
            return self.visit_keyword(keyword);
        }
        if let Some(token) = node.as_token() {
            if token.token_type() == ParseTokenType::end {
                self.visit_semi_nl(node);
                return;
            }
            self.visit_token(token);
            return;
        }
        match node.typ() {
            Type::argument => self.visit_argument(node.as_argument().unwrap()),
            Type::redirection => self.visit_redirection(node.as_redirection().unwrap()),
            Type::variable_assignment => {
                self.visit_variable_assignment(node.as_variable_assignment().unwrap())
            }
            Type::decorated_statement => {
                self.visit_decorated_statement(node.as_decorated_statement().unwrap())
            }
            Type::block_statement => self.visit_block_statement(node.as_block_statement().unwrap()),
            // Default implementation is to just visit children.
            _ => self.visit_children(node),
        }
    }
}

// Given a plain statement node in a parse tree, get the command and return it, expanded
// appropriately for commands. If we succeed, return true.
fn statement_get_expanded_command(
    src: &wstr,
    stmt: &ast::DecoratedStatement,
    ctx: &OperationContext<'_>,
    out_cmd: &mut WString,
) -> bool {
    // Get the command. Try expanding it. If we cannot, it's an error.
    let Some(cmd) = stmt.command.try_source(src) else {
        return false;
    };
    let err = expand_to_command_and_args(cmd, ctx, out_cmd, None, None, false);
    err == ExpandResultCode::ok
}

fn statement_get_expanded_command_ffi(
    src: &CxxWString,
    stmt: &ast::DecoratedStatement,
    ctx: &OperationContext<'_>,
) -> UniquePtr<CxxWString> {
    let mut out_cmd = WString::new();
    if statement_get_expanded_command(src.as_wstr(), stmt, ctx, &mut out_cmd) {
        return out_cmd.to_ffi();
    } else {
        UniquePtr::null()
    }
}

#[cxx::bridge]
#[allow(clippy::needless_lifetimes)] // false positive
mod highlighter_ffi {
    extern "C++" {
        include!("ast.h");
        include!("highlight.h");
        include!("parse_constants.h");
        type highlighter_t = crate::ffi::highlighter_t;
        type Ast = crate::ast::Ast;
        type NodeFfi<'a> = crate::ast::NodeFfi<'a>;
        type DecoratedStatement = crate::ast::DecoratedStatement;
        type OperationContext<'a> = crate::operation_context::OperationContext<'a>;
    }
    extern "Rust" {
        type Highlighter<'a>;
        unsafe fn new_highlighter<'a>(
            companion: Pin<&'a mut highlighter_t>,
            ast: &'a Ast,
        ) -> Box<Highlighter<'a>>;
        #[cxx_name = "visit_children"]
        unsafe fn visit_children_ffi<'a>(self: &mut Highlighter<'a>, node: &'a NodeFfi<'a>);
    }
    extern "Rust" {
        #[cxx_name = "statement_get_expanded_command"]
        fn statement_get_expanded_command_ffi(
            src: &CxxWString,
            stmt: &DecoratedStatement,
            ctx: &OperationContext<'_>,
        ) -> UniquePtr<CxxWString>;
    }
}

fn new_highlighter<'a>(
    companion: Pin<&'a mut highlighter_t>,
    ast: &'a Ast,
) -> Box<Highlighter<'a>> {
    Box::new(Highlighter { companion, ast })
}
impl<'a> Highlighter<'a> {
    fn visit_children_ffi(&mut self, node: &'a NodeFfi<'a>) {
        self.visit_children(node.as_node());
    }
}

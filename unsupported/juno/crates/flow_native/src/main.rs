/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use anyhow::{self, Context};
use juno::ast::{self, node_cast, NodeRc};
use juno::hparser::{self, ParserDialect};
use juno::sema::{DeclKind, LexicalScopeId, Resolution, SemContext};
use juno::{resolve_dependency, sema};
use juno_support::source_manager::SourceId;
use juno_support::NullTerminatedBuf;
use std::fmt;

use std::io::{stdout, BufWriter, Write};
use std::process::exit;
use std::rc::Rc;

/// Print to the output stream if no errors have been seen so far.
/// `$compiler` is a mutable reference to the Compiler struct.
/// `$arg` arguments follow the format pattern used by `format!`.
/// The output must be ASCII.
macro_rules! out {
    ($compiler:expr, $($arg:tt)*) => {{
        $compiler.writer.write_ascii(format_args!($($arg)*));
    }}
}

struct Writer<W: Write> {
    out: BufWriter<W>,
}

impl<W: Write> Writer<W> {
    /// Write to the `out` writer. Used via the `out!` macro. The output must be ASCII.
    fn write_ascii(&mut self, args: fmt::Arguments<'_>) {
        let buf = format!("{}", args);
        debug_assert!(buf.is_ascii(), "Output must be ASCII");
        if let Err(e) = self.out.write_all(buf.as_bytes()) {
            panic!("Failed to write out program: {}", e)
        }
    }
}

/// A type used to represent the result of an intermediate computation stored in
/// a temporary variable in C++. It implements Display to print the name of that
/// variable.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct ValueId(usize);
impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

struct Compiler<W: Write> {
    writer: Writer<W>,
    sem: Rc<SemContext>,
    /// The number of ValueIds that have been created so far. This is also used
    /// to give a unique index to each one.
    num_values: usize,
}

impl<W: Write> Compiler<W> {
    pub fn compile(out: BufWriter<W>, mut ctx: ast::Context, ast: NodeRc, sem: Rc<SemContext>) {
        let lock = ast::GCLock::new(&mut ctx);
        let writer = Writer { out };
        let mut comp = Compiler {
            writer,
            sem,
            num_values: 0,
        };
        comp.gen_program(ast.node(&lock), &lock)
    }

    /// Returns a newly created unique ValueId.
    fn new_value(&mut self) -> ValueId {
        let result = ValueId(self.num_values);
        self.num_values += 1;
        result
    }

    fn gen_program<'gc>(&mut self, node: &'gc ast::Node<'gc>, lock: &'gc ast::GCLock) {
        use ast::*;
        out!(self, "#include \"runtime/FNRuntime.h\"\n");
        self.gen_context();
        out!(self, "int main(){{\n");
        let scope = self.sem.node_scope(NodeRc::from_node(lock, node)).unwrap();
        let Module { body, .. } = node_cast!(Node::Module, node);
        out!(self, "Scope{0} *scope{0}=new Scope{0}();\n", scope);
        for stmt in body.iter() {
            self.gen_stmt(stmt, scope, lock);
            out!(self, ";")
        }
        out!(self, "return 0;\n}}")
    }

    fn gen_context(&mut self) {
        // Forward declare scope structs.
        for i in 0..self.sem.all_scopes().len() {
            out!(self, "struct Scope{};\n", i)
        }
        // Define scope structs.
        for (i, scope) in self.sem.all_scopes().iter().enumerate() {
            out!(self, "struct Scope{}{{\n", i);
            if let Some(parent) = scope.parent_scope {
                out!(self, "Scope{} *parent;\n", parent);
            }
            for decl in &scope.decls {
                out!(self, "FNValue var{}=FNValue::encodeUndefined();\n", decl)
            }
            out!(self, "}};\n");
        }
    }

    fn param_list_for_arg_count(&mut self, count: usize) {
        out!(self, "void *parent_scope");
        for i in 0..count {
            out!(self, ", FNValue param{}", i)
        }
    }

    fn init_scope<'gc>(
        &mut self,
        node: &'gc ast::Node<'gc>,
        scope: LexicalScopeId,
        lock: &'gc ast::GCLock,
    ) -> LexicalScopeId {
        if let Some(new_scope) = self.sem.node_scope(NodeRc::from_node(lock, node)) {
            out!(self, "Scope{0} *scope{0} = new Scope{0}();\n", new_scope);
            out!(self, "scope{}->parent = scope{};\n", new_scope, scope);
            new_scope
        } else {
            scope
        }
    }

    fn gen_member_prop<'gc>(
        &mut self,
        property: &'gc ast::Node<'gc>,
        computed: bool,
        scope: LexicalScopeId,
        lock: &'gc ast::GCLock,
    ) {
        use ast::*;
        if computed {
            out!(self, "->getByVal(");
            self.gen_expr(property, scope, lock);
            out!(self, ")");
        } else {
            let Identifier { name, .. } = node_cast!(Node::Identifier, property);
            out!(self, "->props[\"{}\"]", lock.str(*name))
        }
    }

    fn gen_function_exp<'gc>(
        &mut self,
        params: &'gc ast::NodeList<'gc>,
        block: &'gc ast::Node<'gc>,
        scope: LexicalScopeId,
        lock: &'gc ast::GCLock,
    ) {
        use ast::*;
        out!(
            self,
            "FNValue::encodeClosure(new FNClosure{{(void(*)(void))(+[]("
        );
        self.param_list_for_arg_count(params.len());
        out!(self, "){{");
        let fn_scope = self.sem.node_scope(NodeRc::from_node(lock, block)).unwrap();
        out!(
            self,
            "\nScope{scope} *scope{scope} = (Scope{scope}*)parent_scope;"
        );
        out!(
            self,
            "Scope{fn_scope} *scope{fn_scope} = new Scope{fn_scope}();\n"
        );
        out!(self, "scope{fn_scope}->parent = scope{scope};\n");
        for (i, param) in params.iter().enumerate() {
            self.gen_expr(param, fn_scope, lock);
            out!(self, "=param{i};\n")
        }
        let BlockStatement { body, .. } = node_cast!(Node::BlockStatement, block);
        for stmt in body.iter() {
            self.gen_stmt(stmt, fn_scope, lock);
            out!(self, ";\n");
        }
        out!(self, "}}), scope{scope}}})");
    }

    fn gen_expr<'gc>(
        &mut self,
        node: &'gc ast::Node<'gc>,
        scope: LexicalScopeId,
        lock: &'gc ast::GCLock,
    ) {
        use ast::*;
        match node {
            Node::FunctionExpression(FunctionExpression { params, body, .. }) => {
                self.gen_function_exp(params, body, scope, lock)
            }
            Node::ObjectExpression(ObjectExpression { properties, .. }) => {
                out!(self, "({{FNObject *tmp=new FNObject();\n");
                for prop in properties.iter() {
                    let Property {
                        key,
                        value,
                        computed,
                        ..
                    } = node_cast!(Node::Property, prop);
                    out!(self, "tmp");
                    self.gen_member_prop(key, *computed, scope, lock);
                    out!(self, "=");
                    self.gen_expr(value, scope, lock);
                    out!(self, ";\n");
                }
                out!(self, "FNValue::encodeObject(tmp);}})");
            }
            Node::ArrayExpression(ArrayExpression { elements, .. }) => {
                out!(self, "FNValue::encodeObject(new FNArray({{");
                for elem in elements.iter() {
                    self.gen_expr(elem, scope, lock);
                    out!(self, ",");
                }
                out!(self, "}}))");
            }
            Node::MemberExpression(MemberExpression {
                object,
                property,
                computed,
                ..
            }) => {
                self.gen_expr(object, scope, lock);
                out!(self, ".getObject()");
                self.gen_member_prop(property, *computed, scope, lock);
            }
            Node::CallExpression(CallExpression {
                callee, arguments, ..
            }) => {
                out!(self, "({{FNClosure *tmp=");
                self.gen_expr(callee, scope, lock);
                out!(self, ".getClosure();\nreinterpret_cast<FNValue (*)(");
                self.param_list_for_arg_count(arguments.len());
                out!(self, ")>(tmp->func)(");
                out!(self, "tmp->env");
                for arg in arguments.iter() {
                    out!(self, ", ");
                    self.gen_expr(arg, scope, lock)
                }
                out!(self, ");}})");
            }
            Node::Identifier(..) => {
                let decl_id = match self.sem.ident_decl(&NodeRc::from_node(lock, node)) {
                    Some(Resolution::Decl(decl)) => decl,
                    _ => panic!("Unresolved variable"),
                };
                let decl = self.sem.decl(decl_id);
                match decl.kind {
                    DeclKind::UndeclaredGlobalProperty | DeclKind::GlobalProperty => {
                        out!(self, "global()");
                        self.gen_member_prop(node, false, scope, lock)
                    }
                    _ => {
                        let diff: u32 =
                            self.sem.scope(scope).depth - self.sem.scope(decl.scope).depth;
                        out!(self, "scope{scope}->");
                        for _ in 0..diff {
                            out!(self, "parent->");
                        }
                        out!(self, "var{decl_id}")
                    }
                }
            }
            Node::AssignmentExpression(AssignmentExpression {
                left,
                right,
                operator: op,
                ..
            }) => {
                let type_str = match op {
                    AssignmentExpressionOperator::Assign => "",
                    AssignmentExpressionOperator::PlusAssign
                    | AssignmentExpressionOperator::MinusAssign
                    | AssignmentExpressionOperator::ModAssign
                    | AssignmentExpressionOperator::DivAssign
                    | AssignmentExpressionOperator::MultAssign => ".getNumberRef()",
                    _ => panic!("Unsupported assignment"),
                };
                self.gen_expr(left, scope, lock);
                out!(self, "{type_str}{}", op.as_str());
                self.gen_expr(right, scope, lock);
                out!(self, "{type_str}");
            }
            Node::BinaryExpression(BinaryExpression {
                left,
                right,
                operator: op,
                ..
            }) => {
                let op_str = match op {
                    BinaryExpressionOperator::StrictEquals => "==",
                    BinaryExpressionOperator::StrictNotEquals => "!=",
                    _ => op.as_str(),
                };
                let res_type = match op {
                    BinaryExpressionOperator::LooseEquals
                    | BinaryExpressionOperator::StrictEquals
                    | BinaryExpressionOperator::StrictNotEquals
                    | BinaryExpressionOperator::Less
                    | BinaryExpressionOperator::LessEquals
                    | BinaryExpressionOperator::Greater
                    | BinaryExpressionOperator::GreaterEquals => "Bool",
                    BinaryExpressionOperator::LShift
                    | BinaryExpressionOperator::RShift
                    | BinaryExpressionOperator::Plus
                    | BinaryExpressionOperator::Minus
                    | BinaryExpressionOperator::Mult
                    | BinaryExpressionOperator::Div
                    | BinaryExpressionOperator::Mod => "Number",
                    _ => panic!("Unsupported operator"),
                };
                out!(self, "FNValue::encode{res_type}(");
                self.gen_expr(left, scope, lock);
                out!(self, ".getNumber(){op_str}");
                self.gen_expr(right, scope, lock);
                out!(self, ".getNumber())");
            }
            Node::UpdateExpression(UpdateExpression {
                operator,
                argument,
                prefix,
                ..
            }) => {
                out!(self, "FNValue::encodeNumber(");
                if *prefix {
                    out!(self, "{}", operator.as_str());
                }
                self.gen_expr(argument, scope, lock);
                out!(self, ".getNumberRef()");
                if !*prefix {
                    out!(self, "{}", operator.as_str());
                }
                out!(self, ")");
            }
            Node::NumericLiteral(NumericLiteral { value, .. }) => {
                out!(self, "FNValue::encodeNumber({value})")
            }
            Node::BooleanLiteral(BooleanLiteral { value, .. }) => {
                out!(self, "FNValue::encodeBool({value})")
            }
            Node::StringLiteral(StringLiteral { value, .. }) => {
                let val_str = String::from_utf16_lossy(lock.str_u16(*value));
                out!(self, "FNValue::encodeString(new FNString{{{:?}}})", val_str)
            }
            _ => unimplemented!("Unimplemented expression: {:?}", node.variant()),
        }
    }

    fn gen_stmt<'gc>(
        &mut self,
        node: &'gc ast::Node<'gc>,
        scope: LexicalScopeId,
        lock: &'gc ast::GCLock,
    ) {
        use ast::*;
        match node {
            Node::BlockStatement(BlockStatement { body, .. }) => {
                out!(self, "{{\n");
                let inner_scope = self.init_scope(node, scope, lock);
                for exp in body.iter() {
                    self.gen_stmt(exp, inner_scope, lock)
                }
                out!(self, "}}\n");
            }
            Node::VariableDeclaration(VariableDeclaration { declarations, .. }) => {
                for decl in declarations.iter() {
                    self.gen_stmt(decl, scope, lock)
                }
            }
            Node::VariableDeclarator(VariableDeclarator {
                init: init_opt,
                id: ident,
                ..
            }) => {
                if let Some(init) = init_opt {
                    self.gen_expr(ident, scope, lock);
                    out!(self, "=");
                    self.gen_expr(init, scope, lock)
                }
            }
            Node::FunctionDeclaration(FunctionDeclaration {
                id: ident_opt,
                params,
                body,
                ..
            }) => {
                out!(self, "(");
                if let Some(ident) = ident_opt {
                    self.gen_expr(ident, scope, lock);
                    out!(self, "=");
                }
                self.gen_function_exp(params, body, scope, lock);
                out!(self, ")");
            }

            Node::ReturnStatement(ReturnStatement { argument, .. }) => {
                out!(self, "return ");
                match argument {
                    Some(node) => self.gen_expr(node, scope, lock),
                    None => out!(self, "FNValue::encodeUndefined()"),
                };
                out!(self, ";");
            }
            Node::ExpressionStatement(ExpressionStatement {
                expression: exp, ..
            }) => {
                self.gen_expr(exp, scope, lock);
                out!(self, ";")
            }
            Node::WhileStatement(WhileStatement { test, body, .. }) => {
                out!(self, "while(");
                self.gen_expr(test, scope, lock);
                out!(self, ".getBool()){{\n");
                self.gen_stmt(body, scope, lock);
                out!(self, "\n}}");
            }
            Node::ForStatement(ForStatement {
                init,
                test,
                update,
                body,
                ..
            }) => {
                out!(self, "{{");
                let inner_scope = self.init_scope(node, scope, lock);
                out!(self, "for(");
                if let Some(init) = init {
                    self.gen_stmt(init, inner_scope, lock);
                }
                out!(self, ";");
                if let Some(test) = test {
                    self.gen_expr(test, inner_scope, lock);
                    out!(self, ".getBool()")
                }
                out!(self, ";");
                if let Some(update) = update {
                    self.gen_expr(update, inner_scope, lock);
                }
                out!(self, "){{");
                self.gen_stmt(body, inner_scope, lock);
                out!(self, "}}");
                out!(self, "}}")
            }
            Node::IfStatement(IfStatement {
                test,
                consequent,
                alternate,
                ..
            }) => {
                out!(self, "if(");
                self.gen_expr(test, scope, lock);
                out!(self, ".getBool()){{\n");
                self.gen_stmt(consequent, scope, lock);
                out!(self, "\n}}\nelse{{\n");
                if let Some(alt) = alternate {
                    self.gen_stmt(alt, scope, lock)
                }
                out!(self, "\n}}");
            }
            Node::TryStatement(TryStatement { block, handler, .. }) => {
                out!(self, "try {{");
                self.gen_stmt(block, scope, lock);
                out!(self, "}} catch (FNValue ex){{");
                let handler = if let Some(handler) = handler {
                    handler
                } else {
                    todo!("finally is not implemented");
                };
                let CatchClause { param, body, .. } = node_cast!(Node::CatchClause, handler);
                let new_scope = self.init_scope(handler, scope, lock);
                let BlockStatement { body, .. } = node_cast!(Node::BlockStatement, body);
                if let Some(param) = param {
                    self.gen_expr(param, new_scope, lock);
                    out!(self, "=ex;");
                }
                for stmt in body.iter() {
                    self.gen_stmt(stmt, new_scope, lock);
                    out!(self, ";");
                }
                out!(self, "}}");
            }
            Node::ThrowStatement(ThrowStatement { argument, .. }) => {
                out!(self, "throw ");
                self.gen_expr(argument, scope, lock);
            }
            _ => unimplemented!("Unimplemented statement: {:?}", node.variant()),
        }
    }
}

/// Read the specified file or stdin into a null terminated buffer.
fn read_stdin() -> anyhow::Result<NullTerminatedBuf> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    NullTerminatedBuf::from_reader(&mut handle).context("stdin")
}

fn run() -> anyhow::Result<()> {
    let mut ctx = ast::Context::new();
    let file_id = ctx.sm_mut().add_source("program", read_stdin()?);
    let buf = ctx.sm().source_buffer_rc(file_id);

    let parsed = hparser::ParsedJS::parse(
        hparser::ParserFlags {
            strict_mode: ctx.strict_mode(),
            enable_jsx: false,
            dialect: ParserDialect::JavaScript,
            store_doc_block: false,
        },
        &buf,
    );

    // Convert to Juno AST.
    let (ast, sem) = {
        let resolver = resolve_dependency::DefaultResolver::new(ctx.sm());
        let lock = ast::GCLock::new(&mut ctx);
        let program = &node_cast!(ast::Node::Program, parsed.to_ast(&lock, file_id).unwrap());
        let module = ast::builder::Module::build_template(
            &lock,
            ast::template::Module {
                metadata: ast::TemplateMetadata {
                    phantom: Default::default(),
                    range: program.metadata.range,
                },
                body: program.body,
            },
        );
        (
            NodeRc::from_node(&lock, module),
            sema::resolve_module(&lock, module, SourceId(0), &resolver),
        )
    };
    Compiler::compile(BufWriter::new(stdout()), ctx, ast, Rc::new(sem));
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{:#}", e);
        exit(1);
    }
}

use mime_guess::get_mime_type_str;
use syn::{self, visit::Visit};

use std::{collections::BTreeMap, fmt::Write, mem, path::PathBuf, str};

use crate::parser::{Helper, Node, Ws};

mod ident_buf;
mod visit_derive;
mod visit_each;
mod visits;

use self::ident_buf::Buffer;
pub(crate) use self::visit_derive::{visit_derive, EscapeMode, Print, Struct};
use self::visit_each::find_loop_var;

use crate::append_extension;

pub(crate) fn generate(s: &Struct, ctx: Context) -> String {
    Generator::new(s, ctx).build()
}

pub(self) type Context<'a> = &'a BTreeMap<&'a PathBuf, Vec<Node<'a>>>;

#[derive(Debug, PartialEq)]
pub(self) enum On {
    Each(usize),
    With(usize),
}

enum Writable<'a> {
    Lit(&'a str),
    Expr(String, bool),
}

pub(self) struct Generator<'a> {
    // Options and
    pub(self) s: &'a Struct<'a>,
    // Wrapped expression flag
    pub(self) wrapped: bool,
    // will wrap expression Flag
    pub(self) will_wrap: bool,
    // buffer for tokens
    // TODO: why not use TokenStream
    pub(self) buf_t: String,
    // Scope stack
    pub(self) scp: Vec<Vec<String>>,
    // On State stack
    pub(self) on: Vec<On>,
    // buffer for writable
    buf_w: Vec<Writable<'a>>,
    // Suffix whitespace from the previous literal. Will be flushed to the
    // output buffer unless suppressed by whitespace suppression on the next
    // non-literal.
    next_ws: Option<&'a str>,
    // Whitespace suppression from the previous non-literal. Will be used to
    // determine whether to flush prefix whitespace from the next literal.
    skip_ws: bool,
    ctx: Context<'a>,
    on_path: PathBuf,
    size_hint: usize,
}

impl<'a> Generator<'a> {
    fn new<'n>(s: &'n Struct<'n>, ctx: Context<'n>) -> Generator<'n> {
        Generator {
            s,
            ctx,
            buf_t: String::new(),
            buf_w: vec![],
            next_ws: None,
            on: vec![],
            on_path: s.path.clone(),
            scp: vec![vec!["self".to_string()]],
            skip_ws: false,
            will_wrap: true,
            wrapped: true,
            size_hint: 0,
        }
    }

    // generates the relevant implementations.
    fn build(&mut self) -> String {
        let mut buf = Buffer::new(0);

        let nodes: &[Node] = self.ctx.get(&self.on_path).unwrap();
        self.impl_display(nodes, &mut buf);
        self.impl_template(&mut buf);

        if cfg!(feature = "actix-web") {
            self.responder(&mut buf);
        }

        buf.buf
    }
    // Get mime type
    fn get_mime(&mut self) -> &str {
        let ext = match self.s.path.extension() {
            Some(s) => s.to_str().unwrap(),
            None => "txt",
        };
        get_mime_type_str(ext).expect("valid mime ext")
    }

    // Implement `Display` for the given context struct
    fn impl_template(&mut self, buf: &mut Buffer) {
        buf.writeln(&self.s.implement_head("::wearte::Template"));

        buf.writeln("fn mime() -> &'static str {");
        buf.writeln(&format!("{:?}", self.get_mime()));
        buf.writeln("}");
        buf.writeln("fn size_hint() -> usize {");
        buf.writeln(&format!("{}", self.size_hint));
        buf.writeln("}");
        buf.writeln("}");
    }

    // Implement `Display` for the given context struct.
    fn impl_display(&mut self, nodes: &'a [Node], buf: &mut Buffer) {
        buf.writeln(&self.s.implement_head("::std::fmt::Display"));

        buf.writeln("fn fmt(&self, _fmt: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {");

        let last = buf.buf.len();
        self.handle(nodes, buf);
        self.size_hint = buf.buf.len() - last;
        buf.writeln("Ok(())");

        buf.writeln("}");
        buf.writeln("}");
    }

    // Implement Actix-web's `Responder`.
    fn responder(&mut self, buf: &mut Buffer) {
        buf.writeln(&self.s.implement_head("::wearte::actix_web::Responder"));

        buf.writeln("type Item = ::wearte::actix_web::HttpResponse;");
        buf.writeln("type Error = ::wearte::actix_web::Error;");
        buf.writeln(
            "fn respond_to<S>(self, _req: &::wearte::actix_web::HttpRequest<S>) \
             -> ::std::result::Result<Self::Item, Self::Error> {",
        );

        buf.writeln(
            "self.call()
                .map(|s| Self::Item::Ok().content_type(Self::mime()).body(s))
                .map_err(|_| ::wearte::actix_web::ErrorInternalServerError(\"Template parsing error\"))"
        );

        buf.writeln("}");
        buf.writeln("}");
    }

    /* Helper methods for handling node types */
    fn handle(&mut self, nodes: &'a [Node], buf: &mut Buffer) {
        for n in nodes {
            match n {
                Node::Let(expr) => {
                    self.skip_ws();
                    self.write_buf_writable(buf);
                    self.visit_stmt(expr);
                    buf.writeln(&mem::replace(&mut self.buf_t, String::new()));
                }
                Node::Safe(ws, expr) => {
                    self.visit_expr(expr);
                    self.handle_ws(ws);
                    self.buf_w.push(Writable::Expr(
                        mem::replace(&mut self.buf_t, String::new()),
                        true,
                    ));
                }
                Node::Expr(ws, expr) => {
                    self.wrapped = false;
                    self.visit_expr(expr);
                    self.handle_ws(ws);
                    self.buf_w.push(Writable::Expr(
                        mem::replace(&mut self.buf_t, String::new()),
                        self.wrapped,
                    ))
                }
                Node::Lit(l, lit, r) => self.visit_lit(l, lit, r),
                Node::Helper(h) => self.visit_helper(buf, h),
                Node::Partial(ws, path) => self.visit_partial(buf, ws, path),
                Node::Comment(..) => self.skip_ws(),
            }
        }

        if self.on.is_empty() {
            self.write_buf_writable(buf);
        }
    }

    fn visit_lit(&mut self, lws: &'a str, lit: &'a str, rws: &'a str) {
        debug_assert!(self.next_ws.is_none());
        if !lws.is_empty() {
            if self.skip_ws {
                self.skip_ws = false;
            } else if lit.is_empty() {
                debug_assert!(rws.is_empty());
                self.next_ws = Some(lws);
            } else {
                self.buf_w.push(Writable::Lit(lws));
            }
        }

        if !lit.is_empty() {
            self.buf_w.push(Writable::Lit(lit));
        }

        if !rws.is_empty() {
            self.next_ws = Some(rws);
        }
    }

    fn visit_helper(&mut self, buf: &mut Buffer, h: &'a Helper<'a>) {
        use crate::parser::Helper::*;
        match h {
            Each(ws, e, b) => self.visit_each(buf, ws, e, b),
            If(ifs, elsif, els) => self.visit_if(buf, ifs, elsif, els),
            With(ws, e, b) => self.visit_with(buf, ws, e, b),
            Unless(ws, e, b) => self.visit_unless(buf, ws, e, b),
            Defined(..) => unimplemented!(),
        }
    }

    fn visit_unless(
        &mut self,
        buf: &mut Buffer,
        ws: &'a (Ws, Ws),
        args: &'a syn::Expr,
        nodes: &'a [Node<'a>],
    ) {
        self.handle_ws(&ws.0);
        self.write_buf_writable(buf);

        use syn::Expr::*;
        match args {
            Binary(..) | Call(..) | MethodCall(..) | Index(..) | Field(..) | Path(..)
            | Paren(..) | Macro(..) | Lit(..) | Try(..) => (),
            Unary(syn::ExprUnary { op, .. }) => {
                if let syn::UnOp::Not(..) = op {
                    panic!("Unary negate operator in unless helper, use if helper instead")
                }
            }
            _ => panic!("unless helper not accept some arguments"),
        }

        self.visit_expr(args);
        buf.writeln(&format!(
            "if !({}) {{",
            mem::replace(&mut self.buf_t, String::new())
        ));

        self.scp.push(vec![]);
        self.handle(nodes, buf);
        self.scp.pop();

        self.handle_ws(&ws.1);
        self.write_buf_writable(buf);
        buf.writeln("}");
    }

    fn visit_with(
        &mut self,
        buf: &mut Buffer,
        ws: &'a (Ws, Ws),
        args: &'a syn::Expr,
        nodes: &'a [Node<'a>],
    ) {
        self.handle_ws(&ws.0);
        self.visit_expr(args);
        self.on.push(On::With(self.scp.len()));
        self.scp
            .push(vec![mem::replace(&mut self.buf_t, String::new())]);

        self.handle(nodes, buf);

        self.scp.pop();
        self.on.pop();
        self.handle_ws(&ws.1);
    }

    fn visit_each(
        &mut self,
        buf: &mut Buffer,
        ws: &'a (Ws, Ws),
        args: &'a syn::Expr,
        nodes: &'a [Node<'a>],
    ) {
        self.handle_ws(&ws.0);
        self.write_buf_writable(buf);

        let loop_var = find_loop_var(self.s, self.ctx, self.on_path.clone(), nodes);
        self.visit_expr(args);
        let id = self.scp.len();
        let ctx = if loop_var {
            let ctx = vec![format!("_key_{}", id), format!("_index_{}", id)];
            if let syn::Expr::Range(..) = args {
                buf.writeln(&format!(
                    "for ({}, {}) in ({}).enumerate() {{",
                    ctx[1],
                    ctx[0],
                    &mem::replace(&mut self.buf_t, String::new())
                ));
            } else {
                buf.writeln(&format!(
                    "for ({}, {}) in (&{}).into_iter().enumerate() {{",
                    ctx[1],
                    ctx[0],
                    &mem::replace(&mut self.buf_t, String::new())
                ));
            }
            ctx
        } else {
            let ctx = vec![format!("_key_{}", id)];
            if let syn::Expr::Range(..) = args {
                buf.writeln(&format!(
                    "for {} in {} {{",
                    ctx[0],
                    &mem::replace(&mut self.buf_t, String::new())
                ));
            } else {
                buf.writeln(&format!(
                    "for {} in (&{}).into_iter() {{",
                    ctx[0],
                    &mem::replace(&mut self.buf_t, String::new())
                ));
            }
            ctx
        };
        self.on.push(On::Each(id));
        self.scp.push(ctx);

        self.handle(nodes, buf);
        self.handle_ws(&ws.1);
        self.write_buf_writable(buf);

        self.scp.pop();
        self.on.pop();
        buf.writeln("}");
    }

    fn visit_if(
        &mut self,
        buf: &mut Buffer,
        (pws, cond, block): &'a ((Ws, Ws), syn::Expr, Vec<Node>),
        ifs: &'a [(Ws, syn::Expr, Vec<Node<'a>>)],
        els: &'a Option<(Ws, Vec<Node<'a>>)>,
    ) {
        self.handle_ws(&pws.0);
        self.write_buf_writable(buf);

        self.scp.push(vec![]);
        self.visit_expr(cond);
        buf.writeln(&format!(
            "if {} {{",
            mem::replace(&mut self.buf_t, String::new())
        ));

        self.handle(block, buf);
        self.scp.pop();

        for (ws, cond, block) in ifs {
            self.handle_ws(&ws);
            self.write_buf_writable(buf);

            self.scp.push(vec![]);
            self.visit_expr(cond);
            buf.writeln(&format!(
                "}} else if {} {{",
                mem::replace(&mut self.buf_t, String::new())
            ));

            self.handle(block, buf);
            self.scp.pop();
        }

        if let Some((ws, els)) = els {
            self.handle_ws(ws);
            self.write_buf_writable(buf);

            buf.writeln("} else {");

            self.scp.push(vec![]);
            self.handle(els, buf);
            self.scp.pop();
        }

        self.handle_ws(&pws.1);
        self.write_buf_writable(buf);
        buf.writeln("}");
    }

    fn visit_partial(&mut self, buf: &mut Buffer, ws: &Ws, path: &str) {
        let mut p = self.on_path.clone();
        p.pop();
        p.push(append_extension(&self.s.path, path));
        let nodes = self.ctx.get(&p).unwrap();

        let p = mem::replace(&mut self.on_path, p);

        self.flush_ws(ws);
        self.scp.push(vec![]);
        self.handle(nodes, buf);
        self.scp.pop();
        self.prepare_ws(ws);

        self.on_path = p;
    }

    pub(self) fn write_single_path(&mut self, ident: &str) {
        macro_rules! wrap_and_write {
            ($($t:tt)+) => {{
                self.wrapped = true;
                return write!(self.buf_t, $($t)+).unwrap();
            }};
        }

        if ident == "self" {
            // TODO: partial context
            debug_assert!(!self.scp.is_empty() && !self.scp[0].is_empty());
            write!(self.buf_t, "{}", self.scp[0][0]).unwrap();
        } else if self.scp.iter().all(|v| v.iter().all(|e| ident.ne(e))) {
            if self.on.is_empty() {
                write!(self.buf_t, "{}.{}", self.scp[0][0], ident).unwrap()
            } else {
                if let Some(j) = self.on.iter().rev().find_map(|x| match x {
                    On::Each(j) => Some(j),
                    _ => None,
                }) {
                    match ident {
                        "index0" => wrap_and_write!("{}", self.scp[*j][1]),
                        "index" => wrap_and_write!("({} + 1)", self.scp[*j][1]),
                        "first" => wrap_and_write!("({} == 0)", self.scp[*j][1]),
                        "last" => wrap_and_write!(
                            "(({}).len() == ({} + 1))",
                            self.scp[*j][0],
                            self.scp[*j][1]
                        ),
                        "key" => return write!(self.buf_t, "{}", self.scp[*j][0]).unwrap(),
                        _ => (),
                    }
                }

                match self.on.last() {
                    // self
                    None => write!(self.buf_t, "{}.{}", self.scp[0][0], ident).unwrap(),
                    Some(On::Each(j)) | Some(On::With(j)) => {
                        debug_assert!(self.scp.get(*j).is_some() && !self.scp[*j].is_empty());
                        return write!(self.buf_t, "{}.{}", self.scp[*j][0], ident).unwrap();
                    }
                }
            }
        } else {
            write!(self.buf_t, "{}", ident).unwrap();
        }
    }

    // Write expression buffer and empty
    fn write_buf_writable(&mut self, buf: &mut Buffer) {
        if self.buf_w.is_empty() {
            return;
        }

        let mut buf_lit = String::new();
        if self.buf_w.iter().all(|w| match w {
            Writable::Lit(_) => true,
            _ => false,
        }) {
            for s in mem::replace(&mut self.buf_w, vec![]) {
                if let Writable::Lit(s) = s {
                    buf_lit.write_str(s).unwrap();
                };
            }
            buf.writeln(&format!("_fmt.write_str({:#?})?;", &buf_lit));
            return;
        }

        for s in mem::replace(&mut self.buf_w, vec![]) {
            match s {
                Writable::Lit(s) => buf_lit.write_str(s).unwrap(),
                Writable::Expr(s, wrapped) => {
                    if !buf_lit.is_empty() {
                        buf.writeln(&format!(
                            "_fmt.write_str({:#?})?;",
                            &mem::replace(&mut buf_lit, String::new())
                        ));
                    }

                    buf.writeln(&format!("({}).fmt(_fmt)?;", {
                        use self::EscapeMode::*;
                        match (wrapped, &self.s.escaping) {
                            (true, &Html) | (true, &None) | (false, &None) => s,
                            (false, &Html) => format!("::wearte::MarkupAsStr::from(&{})", s),
                        }
                    }));
                }
            }
        }

        if !buf_lit.is_empty() {
            buf.writeln(&format!("_fmt.write_str({:#?})?;", buf_lit));
        }
    }

    /* Helper methods for dealing with whitespace nodes */

    // Combines `flush_ws()` and `prepare_ws()` to handle both trailing whitespace from the
    // preceding literal and leading whitespace from the succeeding literal.
    fn handle_ws(&mut self, ws: &Ws) {
        self.flush_ws(ws);
        self.prepare_ws(ws);
    }

    // If the previous literal left some trailing whitespace in `next_ws` and the
    // prefix whitespace suppressor from the given argument, flush that whitespace.
    // In either case, `next_ws` is reset to `None` (no trailing whitespace).
    fn flush_ws(&mut self, ws: &Ws) {
        if self.next_ws.is_some() && !ws.0 {
            let val = self.next_ws.unwrap();
            if !val.is_empty() {
                self.buf_w.push(Writable::Lit(val));
            }
        }
        self.next_ws = None;
    }

    // Sets `skip_ws` to match the suffix whitespace suppressor from the given
    // argument, to determine whether to suppress leading whitespace from the
    // next literal.
    fn prepare_ws(&mut self, ws: &Ws) {
        self.skip_ws = ws.1;
    }

    fn skip_ws(&mut self) {
        self.next_ws = None;
        self.skip_ws = true;
    }
}
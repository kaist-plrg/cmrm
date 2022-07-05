use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::Read,
    path::PathBuf,
    sync::Mutex,
};

use lazy_static::lazy_static;
use rustc_data_structures::sync;
use rustc_driver::{Callbacks, RunCompiler};
use rustc_hir::*;
use rustc_lint::{LateContext, LateLintPass, LintContext, LintPass};
use rustc_span::{BytePos, FileName, Span, Symbol};
use rustfix::{Replacement, Snippet, Solution, Suggestion};
use spin::once::Once;

use crate::analysis::{AnalysisSummary, FunctionSummary};

lazy_static! {
    static ref RUSTFIX_SUGGESTIONS: Mutex<HashMap<PathBuf, BTreeMap<i32, Vec<Suggestion>>>> =
        Mutex::new(HashMap::default());
    static ref GLOBAL_DEF_MAP: Mutex<HashMap<String, (String, String)>> =
        Mutex::new(HashMap::default());
    static ref ARRAY_DEF_MAP: Mutex<HashMap<String, (String, Vec<String>)>> =
        Mutex::new(HashMap::default());
    static ref STRUCT_DEF_MAP: Mutex<HashMap<String, Vec<(String, String, String)>>> =
        Mutex::new(HashMap::default());
}

static SUMMARY: Once<AnalysisSummary> = Once::new();

fn global_mutex_map() -> &'static HashMap<String, String> {
    &SUMMARY.get().unwrap().mutex_map
}

fn array_mutex_map() -> &'static HashMap<String, String> {
    &SUMMARY.get().unwrap().array_mutex_map
}

fn struct_mutex_map() -> &'static HashMap<String, String> {
    &SUMMARY.get().unwrap().struct_mutex_map
}

fn function_mutex_map() -> &'static HashMap<String, FunctionSummary> {
    &SUMMARY.get().unwrap().function_map
}

pub fn collect_suggestions(
    args: Vec<String>,
    summary: AnalysisSummary,
) -> HashMap<PathBuf, BTreeMap<i32, Vec<Suggestion>>> {
    SUMMARY.call_once(|| summary);

    let mut callbacks = DriverCallbacks {
        passes: vec![GlobalPass::new],
    };

    let exit_code = rustc_driver::catch_with_exit_code(|| {
        let compiler = RunCompiler::new(&args, &mut callbacks);
        compiler.run()
    });

    assert_eq!(exit_code, 0);

    let mut callbacks = DriverCallbacks {
        passes: vec![RewritePass::new],
    };

    let exit_code = rustc_driver::catch_with_exit_code(|| {
        let compiler = RunCompiler::new(&args, &mut callbacks);
        compiler.run()
    });

    assert_eq!(exit_code, 0);

    let mut suggestions = RUSTFIX_SUGGESTIONS.lock().unwrap();
    std::mem::take(&mut suggestions)
}

pub fn apply_suggestions(path: PathBuf, suggestions: BTreeMap<i32, Vec<Suggestion>>) -> String {
    let ordered_suggestions = suggestions
        .into_values()
        .flatten()
        .collect::<Vec<Suggestion>>();

    let mut file = File::open(path).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();

    rustfix::apply_suggestions(contents.as_str(), &ordered_suggestions).unwrap()
}

type LatePass = dyn for<'tcx> LateLintPass<'tcx> + sync::Send + sync::Sync + 'static;
type LateCallback = fn() -> Box<LatePass>;

struct DriverCallbacks {
    passes: Vec<LateCallback>,
}

impl Callbacks for DriverCallbacks {
    fn config(&mut self, cfg: &mut rustc_interface::Config) {
        let prev_lints = std::mem::replace(&mut cfg.register_lints, None);
        let passes = self.passes.clone();
        cfg.register_lints = Some(Box::new(move |sess, lint_store| {
            for p in passes.clone() {
                lint_store.register_late_pass(p);
            }
            if let Some(lints) = &prev_lints {
                lints(sess, lint_store);
            }
        }));
    }
}

struct GlobalPass;

impl GlobalPass {
    #[allow(clippy::new_ret_no_self)]
    fn new() -> Box<LatePass> {
        Box::new(Self)
    }
}

impl LintPass for GlobalPass {
    fn name(&self) -> &'static str {
        "GlobalPass"
    }
}

impl<'tcx> LateLintPass<'tcx> for GlobalPass {
    fn check_item(&mut self, ctx: &LateContext<'tcx>, i: &'tcx Item<'tcx>) {
        match &i.kind {
            ItemKind::Struct(VariantData::Struct(fs, _), _) => {
                let s = i.ident.name.to_ident_string();
                let v = fs
                    .iter()
                    .filter_map(|f| {
                        let field = f.ident.name.to_ident_string();
                        if let Some(m) = struct_mutex_map().get(&field) {
                            let ty = span_to_string(ctx, f.ty.span);
                            Some((field, ty, m.clone()))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                if !v.is_empty() {
                    STRUCT_DEF_MAP.lock().unwrap().insert(s, v);
                }
            }
            ItemKind::Static(t, _, b) => {
                let name = i.ident.name.to_ident_string();
                match t.kind {
                    // array
                    TyKind::Array(t, _) => {
                        let ty = span_to_string(ctx, t.span);
                        let init = match &ctx.tcx.hir().body(*b).value.kind {
                            ExprKind::Repeat(e, l) => {
                                let e = span_to_string(ctx, e.span);
                                let l = hid_to_string(ctx, l.hir_id).parse().unwrap();
                                vec![e; l]
                            }
                            ExprKind::Array(es) => {
                                es.iter().map(|e| span_to_string(ctx, e.span)).collect()
                            }
                            _ => unreachable!(),
                        };
                        ARRAY_DEF_MAP.lock().unwrap().insert(name, (ty, init));
                    }
                    // global
                    _ => {
                        let ty = span_to_string(ctx, t.span);
                        let init = hid_to_string(ctx, b.hir_id);
                        GLOBAL_DEF_MAP
                            .lock()
                            .unwrap()
                            .insert(name.clone(), (ty, init));
                    }
                }
            }
            _ => (),
        }
    }
}

struct RewritePass {
    depth: i32,
}

impl RewritePass {
    #[allow(clippy::new_ret_no_self)]
    fn new() -> Box<LatePass> {
        Box::new(Self { depth: 0 })
    }
}

impl LintPass for RewritePass {
    fn name(&self) -> &'static str {
        "RewritePass"
    }
}

impl<'tcx> LateLintPass<'tcx> for RewritePass {
    fn check_mod(&mut self, ctx: &LateContext<'tcx>, m: &'tcx Mod<'tcx>, _: Span, _: HirId) {
        let hir = ctx.tcx.hir();
        if m.item_ids
            .iter()
            .any(|i| matches!(hir.item(*i).kind, ItemKind::Fn(_, _, _)))
        {
            let span = m.inner.shrink_to_lo();
            make_suggestion(
                ctx,
                span,
                "".to_string(),
                "use parking_lot::{lock_api::{self, RawMutex}, Mutex, MutexGuard};\nu".to_string(),
                self.depth,
            );
        }
    }

    fn check_item(&mut self, ctx: &LateContext<'tcx>, i: &'tcx Item<'tcx>) {
        match &i.kind {
            ItemKind::Impl(im) => {
                let t = path_to_string(im.of_trait.as_ref().unwrap().path);
                if t == "Copy" {
                    let s = match &im.self_ty.kind {
                        TyKind::Path(QPath::Resolved(_, p)) => path_to_string(p),
                        _ => unreachable!(),
                    };
                    if STRUCT_DEF_MAP.lock().unwrap().contains_key(&s) {
                        let span = i.span;
                        make_suggestion(
                            ctx,
                            span.with_lo(span.lo() - BytePos(9))
                                .with_hi(span.hi() + BytePos(9)),
                            "".to_string(),
                            "".to_string(),
                            self.depth,
                        );
                    }
                }
            }
            ItemKind::Struct(VariantData::Struct(fs, _), _) => {
                let s = i.ident.name.to_ident_string();
                if let Some(v) = STRUCT_DEF_MAP.lock().unwrap().get(&s) {
                    let mut new_structs = String::new();
                    for f in fs.iter() {
                        let name = f.ident.name.to_ident_string();
                        if v.iter().any(|(x, _, _)| *x == name) {
                            let span = f.span;
                            make_suggestion(
                                ctx,
                                span.with_hi(span.hi() + BytePos(1)),
                                "".to_string(),
                                "".to_string(),
                                self.depth,
                            );
                            continue;
                        }
                        let pfs: Vec<_> = v
                            .iter()
                            .filter_map(|(x, t, m)| {
                                if *m == name {
                                    Some(format!("pub {}: {}", x, t))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if !pfs.is_empty() {
                            let st_name = struct_of2(&s, &name);
                            make_suggestion(
                                ctx,
                                f.ty.span,
                                "".to_string(),
                                format!("Mutex<{}>", st_name),
                                self.depth,
                            );
                            let st_body = join(pfs, ", ");
                            let st = format!("\npub struct {} {{ {} }}", st_name, st_body);
                            new_structs.push_str(&st);
                        }
                    }
                    if !new_structs.is_empty() {
                        make_suggestion(
                            ctx,
                            i.span.shrink_to_hi(),
                            "".to_string(),
                            new_structs,
                            self.depth,
                        );
                    }
                }
            }
            ItemKind::Static(t, _, _) => {
                let name = i.ident.name.to_ident_string();

                // global or array
                if global_mutex_map().get(&name).is_some() || array_mutex_map().get(&name).is_some()
                {
                    make_suggestion(ctx, i.span, "".to_string(), "".to_string(), self.depth);
                    remove_attributes(ctx, i, self.depth);
                }

                // mutex
                if span_to_string(ctx, t.span) == "pthread_mutex_t" {
                    let mut decl = String::new();
                    let mut init = String::new();
                    for (x, m) in global_mutex_map() {
                        if *m == name {
                            let (t, i) = GLOBAL_DEF_MAP.lock().unwrap().get(x).unwrap().clone();
                            decl.push_str(format!("pub {}: {}, ", x, t).as_str());
                            init.push_str(format!("{}: {}, ", x, i).as_str());
                        }
                    }
                    let struct_name = struct_of(&name);
                    let code = format!(
                        "
pub struct {0} {{ {1}}}
pub static {2}: Mutex<{0}> = lock_api::Mutex::const_new(
    RawMutex::INIT,
    {0} {{ {3}}}
);",
                        struct_name, decl, name, init
                    );
                    make_suggestion(ctx, i.span, "".to_string(), code, self.depth);
                    remove_attributes(ctx, i, self.depth);
                }

                // mutex array
                if let TyKind::Array(t, _) = t.kind {
                    if span_to_string(ctx, t.span) == "pthread_mutex_t" {
                        let v: Vec<_> = array_mutex_map()
                            .iter()
                            .filter_map(|(x, m)| {
                                if **m == name {
                                    let (t, i) =
                                        ARRAY_DEF_MAP.lock().unwrap().get(x).unwrap().clone();
                                    Some((x, t, i))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        let decls = v.iter().map(|(x, t, _)| format!("pub {}: {}", x, t));
                        let decl = join(decls.collect(), ", ");

                        let struct_name = struct_of(&name);
                        let mut lens: Vec<_> = v.iter().map(|(_, _, i)| i.len()).collect();
                        lens.dedup();
                        assert_eq!(lens.len(), 1);
                        let len = lens[0];
                        let init = join(
                            (0..len)
                                .map(|i| {
                                    let inits =
                                        v.iter().map(|(x, _, init)| format!("{}: {}", x, init[i]));
                                    let init = join(inits.collect(), ", ");
                                    format!(
                                        "
    lock_api::Mutex::const_new(
        RawMutex::INIT,
        {} {{ {} }}
    )",
                                        struct_name, init
                                    )
                                })
                                .collect(),
                            ",",
                        );
                        let code = format!(
                            "
pub struct {0} {{ {1} }}
pub static {2}: [Mutex<{0}>; {3}] = [{4}
];",
                            struct_name, decl, name, len, init
                        );
                        make_suggestion(ctx, i.span, "".to_string(), code, self.depth);
                        remove_attributes(ctx, i, self.depth);
                    }
                }
            }
            _ => (),
        }
    }

    fn check_fn(
        &mut self,
        ctx: &LateContext<'tcx>,
        kind: intravisit::FnKind<'tcx>,
        decl: &'tcx FnDecl<'tcx>,
        body: &'tcx Body<'tcx>,
        span: Span,
        _: HirId,
    ) {
        match kind {
            intravisit::FnKind::ItemFn(id, _, _, _) => {
                let name = id.name.to_ident_string();
                if name == "main" {
                    return;
                }
                let name = if name == "main_0" {
                    "main".to_string()
                } else {
                    name
                };
                let FunctionSummary {
                    entry_mutex: entry,
                    node_mutex: node,
                    ret_mutex: ret,
                } = function_mutex_map().get(&name).unwrap();

                let b = if let ExprKind::Block(b, _) = body.value.kind {
                    b
                } else {
                    unreachable!()
                };
                let stmts = &b.stmts;
                assert!(!stmts.is_empty());

                if !entry.is_empty() {
                    let params = entry
                        .iter()
                        .map(|m| {
                            format!("mut {}: MutexGuard<'static, {}>", guard_of(m), struct_of(m))
                        })
                        .collect();
                    let params = join(params, ", ");
                    let (span, sugg) = if body.params.is_empty() {
                        (
                            ctx.sess()
                                .source_map()
                                .span_through_char(span, '(')
                                .shrink_to_hi(),
                            params,
                        )
                    } else {
                        (
                            decl.inputs.last().unwrap().span.shrink_to_hi(),
                            format!(", {}", params),
                        )
                    };
                    make_suggestion(ctx, span, "".to_string(), sugg, self.depth);
                }

                let local_vars: String = node
                    .iter()
                    .filter(|m| !entry.contains(m))
                    .map(|m| format!("let mut {};\n    ", guard_of(m)))
                    .collect();
                if !local_vars.is_empty() {
                    make_suggestion(
                        ctx,
                        stmts[0].span.shrink_to_lo(),
                        "".to_string(),
                        local_vars,
                        self.depth,
                    );
                }

                if !ret.is_empty() {
                    let mut ret_types = vec![];
                    if let FnRetTy::Return(t) = decl.output {
                        ret_types.push(span_to_string(ctx, t.span));
                    }
                    for m in ret {
                        ret_types.push(format!("MutexGuard<'static, {}>", struct_of(m)));
                    }
                    let ret_type = make_tuple(ret_types);
                    let sugg = if let FnRetTy::Return(_) = decl.output {
                        ret_type
                    } else {
                        format!("-> {} ", ret_type)
                    };
                    make_suggestion(ctx, decl.output.span(), "".to_string(), sugg, self.depth);

                    assert!(b.expr.is_none());
                    let last = stmts.last().unwrap();
                    if let StmtKind::Semi(e) = last.kind {
                        match e.kind {
                            ExprKind::Ret(_) => (),
                            _ => {
                                let ret_vals = ret.iter().map(guard_of).collect();
                                let ret_val = make_tuple(ret_vals);
                                make_suggestion(
                                    ctx,
                                    last.span.shrink_to_hi(),
                                    "".to_string(),
                                    format!("\n    {}", ret_val),
                                    self.depth,
                                );
                            }
                        }
                    } else {
                        unreachable!();
                    }
                }
            }
            _ => (),
        }
    }

    fn check_expr(&mut self, ctx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) {
        match &e.kind {
            ExprKind::Call(func, args) => {
                let f = name(func);
                let arg = || {
                    assert_eq!(args.len(), 1);
                    let arg = &args[0];
                    let arg = unwrap_addr(arg).unwrap();
                    match &arg.kind {
                        ExprKind::Path(_) => {
                            let arg = name(arg).unwrap();
                            let guard = guard_of(&arg);
                            (arg, guard)
                        }
                        ExprKind::Unary(
                            UnOp::Deref,
                            Expr {
                                kind: ExprKind::MethodCall(method, _, args, _),
                                ..
                            },
                        ) => {
                            assert_eq!(method.ident.name.to_ident_string(), "offset");
                            assert_eq!(args.len(), 2);
                            let arr = match &args[0].kind {
                                ExprKind::MethodCall(method, _, args, _) => {
                                    assert_eq!(method.ident.name.to_ident_string(), "as_mut_ptr");
                                    assert_eq!(args.len(), 1);
                                    name(&args[0]).unwrap()
                                }
                                _ => unreachable!(),
                            };
                            let ind = unwrap_cast_recursively(&args[1]);
                            let ind = span_to_string(ctx, ind.span);
                            let arg = format!("{}[{} as usize]", arr, ind);
                            let guard = guard_of(&format!("{}[{}]", arr, ind));
                            (arg, guard)
                        }
                        ExprKind::Field(e, f) => {
                            let field = f.name.to_ident_string();
                            match &e.kind {
                                ExprKind::Path(_) => {
                                    let s = name(e).unwrap();
                                    let arg = format!("{}.{}", s, field);
                                    let guard = guard_of(&arg);
                                    (arg, guard)
                                }
                                ExprKind::Unary(UnOp::Deref, e) => {
                                    let s = name(e).unwrap();
                                    let arg = format!("(*{}).{}", s, field);
                                    let guard = guard_of(&format!("{}->{}", s, field));
                                    (arg, guard)
                                }
                                _ => unreachable!(),
                            }
                        }
                        e => unreachable!("{:?}", e),
                    }
                };
                match f.as_deref() {
                    Some("pthread_mutex_lock") => {
                        let (arg, guard) = arg();
                        make_suggestion(
                            ctx,
                            e.span,
                            "".to_string(),
                            format!("{} = {}.lock()", guard, arg),
                            self.depth,
                        );
                    }
                    Some("pthread_mutex_unlock") => {
                        make_suggestion(
                            ctx,
                            e.span,
                            "".to_string(),
                            format!("drop({})", arg().1),
                            self.depth,
                        );
                    }
                    Some(f) => {
                        if let Some(FunctionSummary {
                            entry_mutex: entry,
                            ret_mutex: ret,
                            ..
                        }) = function_mutex_map().get(f)
                        {
                            if !entry.is_empty() {
                                let guards = entry.iter().map(guard_of).collect();
                                let guards = join(guards, ", ");
                                if let Some(arg) = args.last() {
                                    let span = arg.span.shrink_to_hi();
                                    make_suggestion(
                                        ctx,
                                        span,
                                        "".to_string(),
                                        format!(", {}", guards),
                                        self.depth,
                                    );
                                } else {
                                    let span = ctx
                                        .sess()
                                        .source_map()
                                        .span_through_char(e.span, '(')
                                        .shrink_to_hi();
                                    make_suggestion(ctx, span, "".to_string(), guards, self.depth);
                                }
                            }
                            if !ret.is_empty() {
                                if type_of(ctx, e).is_unit() {
                                    if ret.len() == 1 {
                                        let span = e.span.shrink_to_lo();
                                        make_suggestion(
                                            ctx,
                                            span,
                                            "".to_string(),
                                            format!("{} = ", guard_of(&ret[0])),
                                            self.depth,
                                        );
                                    } else {
                                        let span = e.span.shrink_to_lo();
                                        make_suggestion(
                                            ctx,
                                            span,
                                            "".to_string(),
                                            "{ let tmp = ".to_string(),
                                            self.depth,
                                        );
                                        let span = e.span.shrink_to_hi();
                                        let guards: String = ret
                                            .iter()
                                            .enumerate()
                                            .map(|(i, m)| format!("{} = tmp.{}; ", guard_of(m), i))
                                            .collect();
                                        make_suggestion(
                                            ctx,
                                            span,
                                            "".to_string(),
                                            format!("; {} }}", guards),
                                            self.depth,
                                        );
                                    }
                                } else {
                                    let span = e.span.shrink_to_lo();
                                    make_suggestion(
                                        ctx,
                                        span,
                                        "".to_string(),
                                        "{ let tmp = ".to_string(),
                                        self.depth,
                                    );
                                    let span = e.span.shrink_to_hi();
                                    let guards: String = ret
                                        .iter()
                                        .enumerate()
                                        .map(|(i, m)| format!("{} = tmp.{}; ", guard_of(m), i + 1))
                                        .collect();
                                    make_suggestion(
                                        ctx,
                                        span,
                                        "".to_string(),
                                        format!("; {}tmp.0 }}", guards),
                                        self.depth,
                                    );
                                }
                            }
                        }
                    }
                    _ => (),
                }
            }
            ExprKind::Ret(v_opt) => {
                let hir = ctx.tcx.hir();
                let func = if let Node::Item(item) = hir.get(hir.enclosing_body_owner(e.hir_id)) {
                    item.ident.name.to_ident_string()
                } else {
                    unreachable!()
                };
                let func = if func == "main_0" {
                    "main".to_string()
                } else {
                    func
                };
                let FunctionSummary { ret_mutex: ret, .. } =
                    function_mutex_map().get(&func).unwrap();
                if !ret.is_empty() {
                    let ret_vals = ret.iter().map(guard_of).collect();
                    match v_opt {
                        Some(v) => {
                            let ret_val = join(ret_vals, ", ");
                            make_suggestion(
                                ctx,
                                v.span.shrink_to_lo(),
                                "".to_string(),
                                "(".to_string(),
                                self.depth + 1,
                            );
                            make_suggestion(
                                ctx,
                                v.span.shrink_to_hi(),
                                "".to_string(),
                                format!(", {})", ret_val),
                                self.depth + 1,
                            );
                        }
                        None => {
                            let ret_vals = ret.iter().map(guard_of).collect();
                            let ret_val = make_tuple(ret_vals);
                            make_suggestion(
                                ctx,
                                e.span.shrink_to_hi(),
                                "".to_string(),
                                format!(" {}", ret_val),
                                self.depth,
                            );
                        }
                    }
                }
            }
            ExprKind::Struct(s, fs, _) => {
                let s = match s {
                    QPath::Resolved(_, p) => path_to_string(p),
                    _ => unreachable!(),
                };
                if let Some(v) = STRUCT_DEF_MAP.lock().unwrap().get(&s) {
                    let init_map: HashMap<_, _> = fs
                        .iter()
                        .map(|f| {
                            (
                                f.ident.name.to_ident_string(),
                                span_to_string(ctx, f.expr.span),
                            )
                        })
                        .collect();
                    for f in fs.iter() {
                        let name = f.ident.name.to_ident_string();
                        if v.iter().any(|(x, _, _)| *x == name) {
                            let span = f.span;
                            make_suggestion(
                                ctx,
                                span.with_hi(span.hi() + BytePos(1)),
                                "".to_string(),
                                "".to_string(),
                                self.depth,
                            );
                            continue;
                        }
                        let pfs: Vec<_> = v
                            .iter()
                            .filter_map(|(x, _, m)| {
                                if *m == name {
                                    let i = init_map.get(x).unwrap();
                                    Some(format!("{}: {}", x, i))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if !pfs.is_empty() {
                            let st_name = struct_of2(&s, &name);
                            let st_body = join(pfs, ", ");
                            let st = format!("{} {{ {} }}", st_name, st_body);
                            let ini = format!("lock_api::Mutex::const_new(RawMutex::INIT, {})", st);
                            make_suggestion(ctx, f.expr.span, "".to_string(), ini, self.depth);
                        }
                    }
                }
            }
            // global variable
            ExprKind::Path(_) => {
                if let Some(x) = name(e) {
                    if let Some(m) = global_mutex_map().get(&x) {
                        // disallow struct, function
                        if !m.contains('.') && !type_of(ctx, e).is_fn() {
                            make_suggestion(
                                ctx,
                                e.span,
                                "".to_string(),
                                format!("(*{}).{}", guard_of(m), x),
                                self.depth,
                            );
                        }
                    }
                }
            }
            // array
            ExprKind::Index(a, i) => {
                if let Some(a) = name(a) {
                    let i = unwrap_cast_recursively(i);
                    let i = span_to_string(ctx, i.span);
                    if let Some(m) = array_mutex_map().get(&a) {
                        let m = format!("{}[{}]", m, i);
                        make_suggestion(
                            ctx,
                            e.span,
                            "".to_string(),
                            format!("(*{}).{}", guard_of(&m), a),
                            self.depth,
                        );
                    }
                }
            }
            // struct
            ExprKind::Field(s, f) => {
                let f = f.name.to_ident_string();
                if let Some(m) = struct_mutex_map().get(&f) {
                    let s = match &s.kind {
                        ExprKind::Path(_) => name(s).unwrap(),
                        ExprKind::Unary(UnOp::Deref, s) => name(s).unwrap(),
                        _ => unreachable!(),
                    };
                    let g = guard_of(&format!("{}.{}", s, m));
                    make_suggestion(
                        ctx,
                        e.span,
                        "".to_string(),
                        format!("(*{}).{}", g, f),
                        self.depth,
                    );
                }
            }
            _ => (),
        }
    }
}

// suggestions for left-hand side of expressions
fn make_suggestion(
    ctx: &LateContext<'_>,
    span: Span,
    message: String,
    replacement: String,
    depth: i32,
) {
    make_suggestion_impl(ctx, span, message, replacement, depth)
}

// suggestions for right-hand side of expressions
#[allow(dead_code)]
fn make_suggestion_after(
    ctx: &LateContext<'_>,
    span: Span,
    message: String,
    replacement: String,
    depth: i32,
) {
    make_suggestion_impl(ctx, span, message, replacement, -depth)
}

fn make_suggestion_impl(
    ctx: &LateContext<'_>,
    span: Span,
    message: String,
    replacement: String,
    depth: i32,
) {
    use rustfix::{LinePosition, LineRange};

    let source_map = ctx.sess().source_map();
    let fname = source_map.span_to_filename(span);

    let fname_real = match fname {
        FileName::Real(ref n) => n,
        _ => unreachable!(),
    };

    let file = source_map.get_source_file(&fname).unwrap();
    // get the line and the column numbers
    let lo = file.lookup_file_pos_with_col_display(span.lo());
    let hi = file.lookup_file_pos_with_col_display(span.hi());
    let line_range = LineRange {
        start: LinePosition {
            line: lo.0,
            column: lo.2,
        },
        end: LinePosition {
            line: hi.0,
            column: hi.2,
        },
    };
    // get the char offsets within the file
    let lo_offset = file.original_relative_byte_pos(span.lo()).0;
    let hi_offset = file.original_relative_byte_pos(span.hi()).0;
    let snippet = Snippet {
        file_name: fname.prefer_remapped().to_string(),
        line_range,
        range: (lo_offset as usize)..(hi_offset as usize),
        text: (
            "".into(),
            source_map.span_to_snippet(span).unwrap(),
            "".into(),
        ),
    };

    let mut suggestions = RUSTFIX_SUGGESTIONS.lock().unwrap();

    suggestions
        .entry(fname_real.clone().into_local_path().unwrap())
        .or_default()
        .entry(depth)
        .or_insert_with(Vec::new)
        .push(Suggestion {
            message: "".to_owned(),
            snippets: vec![snippet.clone()],
            solutions: vec![Solution {
                message,
                replacements: vec![Replacement {
                    snippet,
                    replacement,
                }],
            }],
        });
}

fn hid_to_string(ctx: &LateContext<'_>, hid: HirId) -> String {
    span_to_string(ctx, ctx.tcx.hir().span(hid))
}

fn span_to_string(ctx: &LateContext<'_>, span: Span) -> String {
    let source_map = ctx.sess().source_map();
    source_map.span_to_snippet(span).unwrap()
}

fn remove_attributes(ctx: &LateContext<'_>, i: &Item<'_>, depth: i32) {
    let hir = ctx.tcx.hir();
    let attrs = hir.attrs(hir.local_def_id_to_hir_id(i.def_id));
    for a in attrs {
        make_suggestion(ctx, a.span, "".to_string(), "".to_string(), depth);
    }
}

fn type_of<'a, 'b, 'tcx>(
    ctx: &'a LateContext<'b>,
    e: &'tcx Expr<'tcx>,
) -> &'a rustc_middle::ty::TyS<'b> {
    let id = e.hir_id;
    ctx.tcx.typeck(id.owner).node_type(id)
}

fn path_to_symbol<'tcx>(p: &'tcx Path<'tcx>) -> Symbol {
    p.segments.last().unwrap().ident.name
}

fn path_to_string<'tcx>(p: &'tcx Path<'tcx>) -> String {
    path_to_symbol(p).to_ident_string()
}

fn name_symbol<'tcx>(e: &'tcx Expr<'tcx>) -> Option<Symbol> {
    match &e.kind {
        ExprKind::Path(QPath::Resolved(_, p)) => Some(path_to_symbol(p)),
        _ => None,
    }
}

fn name<'tcx>(e: &'tcx Expr<'tcx>) -> Option<String> {
    name_symbol(e).map(|s| s.to_ident_string())
}

fn unwrap_addr<'tcx>(e: &'tcx Expr<'tcx>) -> Option<&'tcx Expr<'tcx>> {
    match &e.kind {
        ExprKind::AddrOf(_, _, e) => Some(e),
        _ => None,
    }
}

fn unwrap_cast_recursively<'tcx>(e: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    match &e.kind {
        ExprKind::Cast(e, _) => unwrap_cast_recursively(e),
        _ => e,
    }
}

#[allow(clippy::ptr_arg)]
fn guard_of(m: &String) -> String {
    if let Some(i) = m.rfind('[') {
        let j = m.rfind(']').unwrap();
        format!("{}_{}_guard", &m[..i], &m[i + 1..j])
    } else if let Some(i) = m.rfind('.') {
        format!("{}_{}_guard", &m[..i], &m[i + 1..])
    } else if let Some(i) = m.rfind("->") {
        format!("{}_{}_guard", &m[..i], &m[i + 2..])
    } else {
        format!("{}_guard", m)
    }
}

fn struct_of(m: &str) -> String {
    format!("{}ProtectedData", m)
}

fn struct_of2(s: &str, m: &str) -> String {
    format!("{}Protected{}", m, s)
}

fn join(mut v: Vec<String>, sep: &str) -> String {
    v.drain(..).intersperse(sep.to_string()).collect()
}

fn make_tuple(mut v: Vec<String>) -> String {
    assert!(!v.is_empty());
    if v.len() == 1 {
        v.pop().unwrap()
    } else {
        format!("({})", join(v, ", "))
    }
}

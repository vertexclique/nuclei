//! Building the macro expansions from the parsed input.
//!
//! The expansions are built as [`Tok`] vectors and converted back to compiler
//! tokens by the thin wrappers in the crate root. [`render`] writes tokens as
//! text; only the expansion tests use it, to compare token streams as written.

use crate::parse::{FnItem, Options, Tok, group, id, joint, num, punct, str_lit};
use proc_macro::Delimiter;

/// The message stamped on a runtime that fails to build inside an expanded
/// `main`, `test` or `bench`.
const FAILED_TO_START: &str = "the nuclei runtime failed to start";

/// Expands `#[nuclei::main]`: the body runs on the runtime's `block_on`.
pub(crate) fn expand_main(item: &FnItem, opts: &Options) -> Vec<Tok> {
    wrapper(item, Vec::new(), Vec::new(), enter(&item.body, opts))
}

/// Expands `#[nuclei::test]`: the body runs on the runtime's `block_on`
/// inside a test function.
pub(crate) fn expand_test(item: &FnItem, opts: &Options) -> Vec<Tok> {
    wrapper(
        item,
        harness_attr("test"),
        Vec::new(),
        enter(&item.body, opts),
    )
}

/// Expands `#[nuclei::bench]`: the harness times runs of the body, each on
/// the runtime's `block_on` (a runtime of its own is built once, before the
/// first). The last run's value is the benchmark's.
pub(crate) fn expand_bench(item: &FnItem, opts: &Options) -> Vec<Tok> {
    let params = vec![
        id(BENCHER),
        punct(':'),
        joint('&'),
        id("mut"),
        id("test"),
        joint(':'),
        joint(':'),
        id("Bencher"),
    ];
    let mut body = Vec::new();
    let run = match opts.threads {
        Some(threads) => {
            // let __nuclei_runtime = <builder>;
            body.extend([id("let"), id(RUNTIME), punct('=')]);
            body.extend(builder(threads));
            body.push(punct(';'));
            let mut run = vec![id(RUNTIME), punct('.'), id("block_on")];
            run.push(async_arg(&item.body));
            run
        }
        None => block_on(&item.body),
    };
    // let mut __nuclei_last = None;
    body.extend([id("let"), id("mut"), id(LAST), punct('=')]);
    body.extend(path(&["core", "option", "Option", "None"]));
    body.push(punct(';'));
    // __nuclei_bencher.iter(|| __nuclei_last = Some(run));
    let mut store = vec![id(LAST), punct('=')];
    store.extend(path(&["core", "option", "Option", "Some"]));
    store.push(group(Delimiter::Parenthesis, run));
    let mut closure = vec![joint('|'), punct('|')];
    closure.extend(store);
    body.extend([id(BENCHER), punct('.'), id("iter")]);
    body.push(group(Delimiter::Parenthesis, closure));
    body.push(punct(';'));
    // __nuclei_last.expect("...")
    body.extend([id(LAST), punct('.'), id("expect")]);
    body.push(group(Delimiter::Parenthesis, vec![str_lit(NEVER_RAN)]));
    wrapper(item, harness_attr("bench"), params, body)
}

/// The names the bench expansion binds, out of the way of the body's.
const BENCHER: &str = "__nuclei_bencher";
const RUNTIME: &str = "__nuclei_runtime";
const LAST: &str = "__nuclei_last";

/// The message of a benchmark whose harness never ran its body.
const NEVER_RAN: &str = "the benchmark harness never ran the body";

/// Builds the wrapper function: harness attribute, the user's attributes and
/// visibility, the name, the parameter list, the return type and the body
/// that enters the runtime.
fn wrapper(item: &FnItem, leading_attrs: Vec<Tok>, params: Vec<Tok>, enter: Vec<Tok>) -> Vec<Tok> {
    let mut out = leading_attrs;
    out.extend(item.attrs.iter().cloned());
    out.extend(item.vis.iter().cloned());
    out.push(id("fn"));
    out.push(Tok::Ident(item.name.clone(), item.name_span));
    out.push(group(Delimiter::Parenthesis, params));
    out.extend(item.ret.iter().cloned());
    out.push(group(Delimiter::Brace, enter));
    out
}

/// The `#[::core::prelude::v1::<name>]` attribute that marks the wrapper as
/// a test or a benchmark.
fn harness_attr(name: &str) -> Vec<Tok> {
    vec![
        joint('#'),
        group(Delimiter::Bracket, path(&["core", "prelude", "v1", name])),
    ]
}

/// What runs the async body: the process default runtime's `block_on`, or a
/// runtime built with the given worker thread count when `threads` is set.
fn enter(body: &[Tok], opts: &Options) -> Vec<Tok> {
    let Some(threads) = opts.threads else {
        return block_on(body);
    };
    let mut out = builder(threads);
    out.push(punct('.'));
    out.push(id("block_on"));
    out.push(async_arg(body));
    out
}

/// `::nuclei::Runtime::builder().worker_threads(N).build().expect("...")`.
fn builder(threads: u32) -> Vec<Tok> {
    let mut out = path(&["nuclei", "Runtime", "builder"]);
    out.push(group(Delimiter::Parenthesis, Vec::new()));
    out.push(punct('.'));
    out.push(id("worker_threads"));
    out.push(group(
        Delimiter::Parenthesis,
        vec![num(&threads.to_string())],
    ));
    out.push(punct('.'));
    out.push(id("build"));
    out.push(group(Delimiter::Parenthesis, Vec::new()));
    out.push(punct('.'));
    out.push(id("expect"));
    out.push(group(
        Delimiter::Parenthesis,
        vec![str_lit(FAILED_TO_START)],
    ));
    out
}

/// `::nuclei::block_on(async { body })`.
fn block_on(body: &[Tok]) -> Vec<Tok> {
    let mut out = path(&["nuclei", "block_on"]);
    out.push(async_arg(body));
    out
}

/// `(async { body })`: the argument of a `block_on`.
fn async_arg(body: &[Tok]) -> Tok {
    group(
        Delimiter::Parenthesis,
        vec![id("async"), group(Delimiter::Brace, body.to_vec())],
    )
}

/// An absolute path, written with a leading `::`.
fn path(parts: &[&str]) -> Vec<Tok> {
    let mut out = vec![joint(':'), joint(':')];
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(joint(':'));
            out.push(joint(':'));
        }
        out.push(id(part));
    }
    out
}

/// Writes tokens as text, close to how they were written in source: tokens
/// glued in the source stay glued, call parentheses sit tight on their name,
/// and path separators, commas and semicolons take no leading space.
#[cfg(test)]
pub(crate) fn render(toks: &[Tok]) -> String {
    let mut out = String::new();
    render_into(toks, &mut out);
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
fn render_into(toks: &[Tok], out: &mut String) {
    for (i, tok) in toks.iter().enumerate() {
        if i > 0 && separated(&toks[i - 1], tok) {
            out.push(' ');
        }
        match tok {
            Tok::Ident(s, _) => out.push_str(s),
            Tok::Punct(c, _, _) => out.push(*c),
            Tok::Literal(s, _) => out.push_str(s),
            Tok::Group(d, inner, _) => {
                let (open, close, spaced) = match d {
                    Delimiter::Parenthesis => ("(", ")", false),
                    Delimiter::Brace => ("{", "}", true),
                    Delimiter::Bracket => ("[", "]", false),
                    Delimiter::None => (" ", " ", false),
                };
                out.push_str(open);
                if spaced && !inner.is_empty() {
                    out.push(' ');
                }
                render_into(inner, out);
                if spaced && !inner.is_empty() {
                    out.push(' ');
                }
                out.push_str(close);
            }
        }
    }
}

/// Whether a space goes between two adjacent tokens.
#[cfg(test)]
fn separated(prev: &Tok, next: &Tok) -> bool {
    if matches!(next, Tok::Punct(':' | '.' | ',' | ';', _, _))
        || matches!(
            (prev, next),
            (Tok::Ident(..), Tok::Group(Delimiter::Parenthesis, _, _))
                | (Tok::Ident(..), Tok::Punct('!', _, _))
                | (
                    Tok::Punct('!', _, _),
                    Tok::Group(Delimiter::Parenthesis, _, _)
                )
                | (Tok::Group(..), Tok::Punct('.', _, _))
        )
    {
        return false;
    }
    !matches!(prev, Tok::Punct(_, true, _) | Tok::Punct('.', _, _))
}

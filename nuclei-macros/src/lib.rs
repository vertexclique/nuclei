//! Attribute macros that enter the nuclei runtime.
//!
//! Use them through the re-exports in the nuclei crate. `#[nuclei::main]`
//! wraps the body of an async `main` function in the runtime's `block_on`,
//! and `#[nuclei::test]` and `#[nuclei::bench]` do the same for test and
//! benchmark functions. Every macro accepts these options, comma separated,
//! each at most once:
//!
//! - `threads = N`: a runtime of its own with N worker threads (a plain
//!   decimal number); 0 runs everything on the `block_on` thread.
//! - `start_paused = true`: a runtime of its own whose clock starts paused
//!   (see `nuclei::time::pause`), with no workers unless `threads` says
//!   otherwise, so every task runs on the thread that runs the body.
//! - `crate = "path"`: the path nuclei is reached by, for code that
//!   re-exports it (`crate = "my_crate::nuclei"`); `::nuclei` otherwise.
//!
//! Without `threads` or `start_paused` the process default runtime starts
//! with zero configuration. The expansions call the runtime entry points of
//! the nuclei crate: `block_on` on the default runtime, or
//! `Runtime::builder().worker_threads(N)` (and `.start_paused(true)`)
//! `.build()` finished with `block_on`.
//!
//! # `#[nuclei::main]`
//!
//! ```no_run
//! use std::net::TcpListener;
//! use nuclei::Handle;
//!
//! #[nuclei::main]
//! async fn main() -> std::io::Result<()> {
//!     let listener = Handle::<TcpListener>::bind("0.0.0.0:7000")?;
//!     loop {
//!         let (stream, _) = listener.accept().await?;
//!         nuclei::spawn(async move {
//!             let mut buf = [0u8; 1024];
//!             while let Ok(n @ 1..) = stream.recv(&mut buf).await {
//!                 if stream.send(&buf[..n]).await.is_err() {
//!                     break;
//!                 }
//!             }
//!         })
//!         .detach();
//!     }
//! }
//! ```
//!
//! With an explicit worker thread count:
//!
//! ```
//! #[nuclei::main(threads = 2)]
//! async fn main() {
//!     println!("two worker threads");
//! }
//! ```
//!
//! # `#[nuclei::test]`
//!
//! ```
//! #[nuclei::test]
//! async fn echoes() -> std::io::Result<()> {
//!     assert_eq!(2 * 2, 4);
//!     Ok(())
//! }
//! ```
//!
//! On a paused clock, code that sleeps runs at once:
//!
//! ```
//! use std::time::Duration;
//!
//! #[nuclei::test(start_paused = true)]
//! async fn waits_a_day() {
//!     let start = nuclei::time::Instant::now();
//!     nuclei::time::sleep(Duration::from_secs(86_400)).await;
//!     assert_eq!(start.elapsed(), Duration::from_secs(86_400));
//! }
//! ```
//!
//! # `#[nuclei::bench]`
//!
//! Benchmarks need the nightly test harness, so the crate using them writes
//! `#![feature(test)]` and `extern crate test;` at its root (which is why
//! this example is not compiled here; the nightly CI job builds and runs
//! the macro in `tests/macros_bench.rs`). The harness times runs of the
//! body, each on `block_on`; with `threads = N` the runtime is built once,
//! before the first.
//!
//! ```ignore
//! #![feature(test)]
//! extern crate test;
//!
//! #[nuclei::bench]
//! async fn spawns_one() {
//!     nuclei::spawn(async {}).detach();
//! }
//! ```

#![forbid(unsafe_code, future_incompatible, rust_2018_idioms)]
#![deny(missing_debug_implementations, missing_docs, nonstandard_style)]

mod expand;
mod parse;
#[cfg(test)]
mod tests;

use parse::{Error, Macro};
use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

/// Enables an async main function.
///
/// The body of the async function runs on the runtime's `block_on`; with
/// `threads = N` the runtime is built with N worker threads, where 0 runs
/// everything on the `block_on` thread.
///
/// # Examples
///
/// ```
/// #[nuclei::main]
/// async fn main() -> std::io::Result<()> {
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    run(Macro::Main, attr, item)
}

/// Enables an async test function.
///
/// The body of the async function runs on the runtime's `block_on`; with
/// `threads = N` the test builds its own runtime with N worker threads,
/// where 0 runs everything on the `block_on` thread.
///
/// # Examples
///
/// ```
/// #[nuclei::test]
/// async fn adds_up() -> std::io::Result<()> {
///     assert_eq!(2 + 2, 4);
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    run(Macro::Test, attr, item)
}

/// Enables an async benchmark function.
///
/// The harness times runs of the body of the async function, each on the
/// runtime's `block_on` (with `threads = N` the runtime is built once,
/// before the first); the benchmark function itself takes the harness, so
/// the annotated function takes no parameters. Benchmarks need the nightly
/// test harness: write `#![feature(test)]` and `extern crate test;` at the
/// crate root (so the example is not compiled here).
///
/// # Examples
///
/// ```ignore
/// #[nuclei::bench]
/// async fn spawns_one() {
///     nuclei::spawn(async {}).detach();
/// }
/// ```
#[proc_macro_attribute]
pub fn bench(attr: TokenStream, item: TokenStream) -> TokenStream {
    run(Macro::Bench, attr, item)
}

/// One shared body for the three entry points: convert, parse, expand,
/// convert back; any error becomes a compile error at its span.
fn run(mac: Macro, attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = to_toks(attr);
    let item = to_toks(item);
    expand_toks(mac, &attr, &item)
        .and_then(|toks| from_toks(&toks))
        .unwrap_or_else(compile_error)
}

fn expand_toks(
    mac: Macro,
    attr: &[parse::Tok],
    item: &[parse::Tok],
) -> Result<Vec<parse::Tok>, Error> {
    let opts = parse::parse_options(attr, mac)?;
    let fn_item = parse::parse_fn_item(item, mac)?;
    match mac {
        Macro::Main => {
            parse::require_main_name(&fn_item)?;
            Ok(expand::expand_main(&fn_item, &opts))
        }
        Macro::Test => Ok(expand::expand_test(&fn_item, &opts)),
        Macro::Bench => Ok(expand::expand_bench(&fn_item, &opts)),
    }
}

/// Reads compiler tokens into the crate's own representation, keeping each
/// token's span.
fn to_toks(ts: TokenStream) -> Vec<parse::Tok> {
    ts.into_iter().map(to_tok).collect()
}

fn to_tok(tt: TokenTree) -> parse::Tok {
    match tt {
        TokenTree::Ident(i) => parse::Tok::Ident(i.to_string(), Some(i.span())),
        TokenTree::Punct(p) => {
            parse::Tok::Punct(p.as_char(), p.spacing() == Spacing::Joint, Some(p.span()))
        }
        TokenTree::Literal(l) => parse::Tok::Literal(l.to_string(), Some(l.span())),
        TokenTree::Group(g) => {
            parse::Tok::Group(g.delimiter(), to_toks(g.stream()), Some(g.span()))
        }
    }
}

/// Writes the crate's own tokens back to the compiler, stamping generated
/// tokens with the call site and re-emitted tokens with their original span.
fn from_toks(toks: &[parse::Tok]) -> Result<TokenStream, Error> {
    let mut out = Vec::with_capacity(toks.len());
    for tok in toks {
        out.push(from_tok(tok)?);
    }
    Ok(out.into_iter().collect())
}

fn from_tok(tok: &parse::Tok) -> Result<TokenTree, Error> {
    let call_site = Span::call_site;
    match tok {
        parse::Tok::Ident(name, span) => {
            let span = span.unwrap_or_else(call_site);
            let ident = match name.strip_prefix("r#") {
                Some(raw) => Ident::new_raw(raw, span),
                None => Ident::new(name, span),
            };
            Ok(TokenTree::Ident(ident))
        }
        parse::Tok::Punct(c, joint, span) => {
            let mut p = Punct::new(
                *c,
                if *joint {
                    Spacing::Joint
                } else {
                    Spacing::Alone
                },
            );
            p.set_span(span.unwrap_or_else(call_site));
            Ok(TokenTree::Punct(p))
        }
        parse::Tok::Literal(text, span) => {
            // A literal's source text parses back to exactly one token; this
            // branch is unreachable for round-tripped and generated literals.
            let stream: TokenStream = text.parse().map_err(|_| reemit_error(*span))?;
            let mut tokens = stream.into_iter().collect::<Vec<_>>();
            let mut token = match (tokens.pop(), tokens.is_empty()) {
                (Some(t), true) => t,
                _ => return Err(reemit_error(*span)),
            };
            token.set_span(span.unwrap_or_else(call_site));
            Ok(token)
        }
        parse::Tok::Group(d, inner, span) => {
            let mut g = Group::new(*d, from_toks(inner)?);
            g.set_span(span.unwrap_or_else(call_site));
            Ok(TokenTree::Group(g))
        }
    }
}

fn reemit_error(span: Option<Span>) -> Error {
    Error::at(
        "a literal in the input could not be re-emitted; this is a bug in nuclei-macros, please report it",
        span,
    )
}

/// Emits `compile_error!` with the message, every token carrying the error's
/// span so the error is reported where it happened.
fn compile_error(e: Error) -> TokenStream {
    let span = e.span.unwrap_or_else(Span::call_site);
    let mut bang = Punct::new('!', Spacing::Alone);
    bang.set_span(span);
    let mut literal = Literal::string(&e.message);
    literal.set_span(span);
    let mut parens = Group::new(
        Delimiter::Parenthesis,
        TokenStream::from(TokenTree::Literal(literal)),
    );
    parens.set_span(span);
    let mut semi = Punct::new(';', Spacing::Alone);
    semi.set_span(span);
    [
        TokenTree::Ident(Ident::new("compile_error", span)),
        TokenTree::Punct(bang),
        TokenTree::Group(parens),
        TokenTree::Punct(semi),
    ]
    .into_iter()
    .collect()
}

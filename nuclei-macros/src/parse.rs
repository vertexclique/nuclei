//! The token representation and the hand-written parsers.
//!
//! The macros parse their input into [`Tok`] values instead of touching the
//! compiler's types directly, so the parsing logic can be unit tested:
//! spans only exist while a macro expands, so every token carries its span
//! optionally and tests build tokens without one.

use proc_macro::{Delimiter, Span};

/// One token of a macro input or expansion.
#[derive(Clone, Debug)]
pub(crate) enum Tok {
    /// An identifier or keyword, as source text; raw identifiers keep their
    /// `r#` prefix.
    Ident(String, Option<Span>),
    /// A punctuation character; the flag says the next token was glued to it
    /// in the source.
    Punct(char, bool, Option<Span>),
    /// A literal, as source text.
    Literal(String, Option<Span>),
    /// A delimited group with its inner tokens.
    Group(Delimiter, Vec<Tok>, Option<Span>),
}

impl Tok {
    /// The span the token came from, when the compiler provided one.
    pub(crate) fn span(&self) -> Option<Span> {
        match self {
            Tok::Ident(_, s) | Tok::Punct(_, _, s) | Tok::Literal(_, s) | Tok::Group(_, _, s) => *s,
        }
    }
}

/// An identifier or keyword token.
pub(crate) fn id(name: &str) -> Tok {
    Tok::Ident(name.to_string(), None)
}

/// A punctuation token separated from its neighbours.
pub(crate) fn punct(c: char) -> Tok {
    Tok::Punct(c, false, None)
}

/// A punctuation token glued to the next one, such as either colon of `::`.
pub(crate) fn joint(c: char) -> Tok {
    Tok::Punct(c, true, None)
}

/// A numeric literal token.
pub(crate) fn num(text: &str) -> Tok {
    Tok::Literal(text.to_string(), None)
}

/// A string literal token built from plain text.
pub(crate) fn str_lit(text: &str) -> Tok {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    Tok::Literal(format!("\"{escaped}\""), None)
}

/// A group token with the given inner tokens.
pub(crate) fn group(d: Delimiter, inner: Vec<Tok>) -> Tok {
    Tok::Group(d, inner, None)
}

/// A macro error: one sentence for the user and the span it points at.
#[derive(Clone, Debug)]
pub(crate) struct Error {
    /// The message, saying what happened and what to do.
    pub(crate) message: String,
    /// Where the message points; absent means the whole invocation.
    pub(crate) span: Option<Span>,
}

impl Error {
    /// An error pointing at the given span, when there is one.
    pub(crate) fn at(message: impl Into<String>, span: Option<Span>) -> Error {
        Error {
            message: message.into(),
            span,
        }
    }
}

/// Which macro an input is parsed for; the accepted shapes are the same and
/// the error messages name the macro.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Macro {
    /// `#[nuclei::main]`.
    Main,
    /// `#[nuclei::test]`.
    Test,
    /// `#[nuclei::bench]`.
    Bench,
}

impl Macro {
    /// The macro's name, as written in its error messages.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Macro::Main => "main",
            Macro::Test => "test",
            Macro::Bench => "bench",
        }
    }
}

/// The options a macro invocation carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Options {
    /// The `threads` value: the worker thread count, where 0 runs everything
    /// on the `block_on` thread. `None` leaves the runtime at its zero
    /// configuration default.
    pub(crate) threads: Option<u32>,
}

/// The parsed shape of the function a macro is applied to.
#[derive(Clone, Debug)]
pub(crate) struct FnItem {
    /// The outer attributes of the function, re-emitted on the wrapper.
    pub(crate) attrs: Vec<Tok>,
    /// The visibility of the function, re-emitted on the wrapper.
    pub(crate) vis: Vec<Tok>,
    /// The function's name.
    pub(crate) name: String,
    /// The span of the name, for errors about it.
    pub(crate) name_span: Option<Span>,
    /// The tokens between the parameter list and the body: the return type
    /// and, should the user write one, a trailing where clause.
    pub(crate) ret: Vec<Tok>,
    /// The tokens inside the function's body.
    pub(crate) body: Vec<Tok>,
}

/// Parses the attribute arguments: empty, or `threads = <number>`, with an
/// optional trailing comma.
pub(crate) fn parse_options(toks: &[Tok], mac: Macro) -> Result<Options, Error> {
    let mut toks = toks;
    // Tolerate the arguments arriving wrapped in one empty parenthesis group.
    if let [Tok::Group(Delimiter::Parenthesis, inner, _)] = toks
        && inner.is_empty()
    {
        toks = &[];
    }

    let mut threads = None;
    while !toks.is_empty() {
        let (name_span, name) = match &toks[0] {
            Tok::Ident(name, span) => (*span, name.as_str()),
            t => {
                return Err(Error::at(
                    "write the option as `threads = <number>`",
                    t.span(),
                ));
            }
        };
        if name != "threads" {
            return Err(Error::at(
                format!(
                    "unknown option `{name}`; #[nuclei::{}] accepts only `threads`",
                    mac.name()
                ),
                name_span,
            ));
        }
        match toks.get(1) {
            Some(Tok::Punct('=', _, _)) => {}
            _ => {
                return Err(Error::at(
                    "write the option as `threads = <number>`",
                    name_span,
                ));
            }
        }
        let (value_span, value) = match toks.get(2) {
            Some(Tok::Literal(text, span)) => (*span, text.as_str()),
            Some(t) => return Err(Error::at(thread_number_message(), t.span())),
            None => return Err(Error::at(thread_number_message(), toks[1].span())),
        };
        let Some(digits) = plain_decimal(value) else {
            return Err(Error::at(thread_number_message(), value_span));
        };
        let Ok(count) = digits.parse::<u32>() else {
            return Err(Error::at(
                "threads is too large; write a smaller number",
                value_span,
            ));
        };
        if threads.is_some() {
            return Err(Error::at(
                "threads is set more than once; write it a single time",
                name_span,
            ));
        }
        threads = Some(count);
        toks = &toks[3..];
        match toks.split_first() {
            None => break,
            Some((Tok::Punct(',', _, _), rest)) => toks = rest,
            Some((t, _)) => return Err(Error::at("separate options with a comma", t.span())),
        }
    }
    Ok(Options { threads })
}

fn thread_number_message() -> &'static str {
    "give threads a number written as a plain decimal integer, for example `threads = 4`"
}

/// Reads a plain decimal integer, digits and `_` separators only, with the
/// separators removed; anything else is not a number this macro accepts.
fn plain_decimal(text: &str) -> Option<String> {
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits)
}

/// Parses the annotated item: an async function with no parameters and no
/// generics, of the shape `attrs vis async fn name() ret? { body }`.
pub(crate) fn parse_fn_item(toks: &[Tok], mac: Macro) -> Result<FnItem, Error> {
    let only_async = |span: Option<Span>| {
        Error::at(
            format!(
                "only an async function can be tagged with #[nuclei::{}]",
                mac.name()
            ),
            span,
        )
    };

    let mut i = 0;
    let mut attrs = Vec::new();
    while matches!(toks.get(i), Some(Tok::Punct('#', _, _))) {
        match toks.get(i + 1) {
            Some(Tok::Group(Delimiter::Bracket, _, _)) => {
                attrs.push(toks[i].clone());
                attrs.push(toks[i + 1].clone());
                i += 2;
            }
            Some(Tok::Punct('!', _, span)) => {
                return Err(Error::at(
                    "inner attributes are not supported here; use outer attributes",
                    *span,
                ));
            }
            t => {
                let span = t.and_then(|t| t.span()).or_else(|| toks[i].span());
                return Err(Error::at("expected an attribute after `#`", span));
            }
        }
    }

    let mut vis = Vec::new();
    if let Some(Tok::Ident(word, _)) = toks.get(i)
        && word == "pub"
    {
        vis.push(toks[i].clone());
        i += 1;
        if let Some(Tok::Group(Delimiter::Parenthesis, _, _)) = toks.get(i) {
            vis.push(toks[i].clone());
            i += 1;
        }
    }

    match toks.get(i) {
        Some(Tok::Ident(word, span)) if word == "async" => i += 1,
        Some(Tok::Ident(word, span))
            if matches!(
                word.as_str(),
                "fn" | "const" | "unsafe" | "extern" | "auto" | "default"
            ) =>
        {
            return Err(Error::at(
                "the async keyword is missing from the function declaration",
                *span,
            ));
        }
        Some(t) => return Err(only_async(t.span())),
        None => return Err(only_async(None)),
    }
    match toks.get(i) {
        Some(Tok::Ident(word, _)) if word == "fn" => i += 1,
        Some(t) => return Err(only_async(t.span())),
        None => return Err(only_async(None)),
    }

    let (name, name_span) = match toks.get(i) {
        Some(Tok::Ident(name, span)) => (name.clone(), *span),
        Some(t) => return Err(Error::at("expected the function name", t.span())),
        None => return Err(only_async(None)),
    };
    i += 1;

    if matches!(toks.get(i), Some(Tok::Punct('<', _, _))) {
        return Err(Error::at(
            "generic functions are not supported; write the function without type parameters",
            toks[i].span(),
        ));
    }

    match toks.get(i) {
        Some(Tok::Group(Delimiter::Parenthesis, inner, span)) => {
            if !inner.is_empty() {
                return Err(Error::at(
                    format!("#[nuclei::{}] functions take no parameters", mac.name()),
                    *span,
                ));
            }
            i += 1;
        }
        Some(t) => {
            return Err(Error::at(
                "expected the function's parameter list",
                t.span(),
            ));
        }
        None => return Err(only_async(None)),
    }

    let mut ret = Vec::new();
    let body;
    loop {
        match toks.get(i) {
            Some(Tok::Group(Delimiter::Brace, inner, _)) => {
                body = inner.clone();
                i += 1;
                break;
            }
            Some(t) => {
                ret.push(t.clone());
                i += 1;
            }
            None => {
                let span = ret.last().and_then(|t| t.span());
                return Err(Error::at("expected the function body", span));
            }
        }
    }
    if let Some(t) = toks.get(i) {
        return Err(Error::at(
            "there are extra tokens after the function body; remove them",
            t.span(),
        ));
    }

    Ok(FnItem {
        attrs,
        vis,
        name,
        name_span,
        ret,
        body,
    })
}

/// Checks the function is named `main`; only `#[nuclei::main]` requires it.
pub(crate) fn require_main_name(item: &FnItem) -> Result<(), Error> {
    if item.name != "main" {
        return Err(Error::at(
            "only the main function can be tagged with #[nuclei::main]",
            item.name_span,
        ));
    }
    Ok(())
}

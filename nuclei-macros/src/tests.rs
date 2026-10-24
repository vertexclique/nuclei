//! Unit tests over the crate's own token representation: every accepted
//! option and item shape, every rejected shape with its message, and the
//! expansion of each macro compared to the expected token text.

use crate::expand::{expand_bench, expand_main, expand_test, render};
use crate::parse::{Error, Macro, Options, Tok, group, id, joint, num, punct, str_lit};
use proc_macro::Delimiter::{Brace, Bracket, Parenthesis};

fn item(name: &str, body: Vec<Tok>) -> Vec<Tok> {
    item_ret(name, Vec::new(), body)
}

fn item_ret(name: &str, ret: Vec<Tok>, body: Vec<Tok>) -> Vec<Tok> {
    let mut toks = vec![
        id("async"),
        id("fn"),
        id(name),
        group(Parenthesis, Vec::new()),
    ];
    toks.extend(ret);
    toks.push(group(Brace, body));
    toks
}

fn ret_io_result() -> Vec<Tok> {
    vec![
        joint('-'),
        punct('>'),
        id("std"),
        joint(':'),
        joint(':'),
        id("io"),
        joint(':'),
        joint(':'),
        id("Result"),
        punct('<'),
        group(Parenthesis, Vec::new()),
        punct('>'),
    ]
}

fn opts(toks: &[Tok]) -> Result<Options, Error> {
    crate::parse::parse_options(toks, Macro::Main)
}

fn options_error(toks: &[Tok]) -> String {
    match opts(toks) {
        Ok(_) => panic!("expected the options to be rejected"),
        Err(e) => e.message,
    }
}

fn parsed(toks: &[Tok]) -> crate::parse::FnItem {
    crate::parse::parse_fn_item(toks, Macro::Main).expect("expected the item to parse")
}

fn item_error(toks: &[Tok]) -> String {
    match crate::parse::parse_fn_item(toks, Macro::Main) {
        Ok(_) => panic!("expected the item to be rejected"),
        Err(e) => e.message,
    }
}

fn threads_eq(toks: &[Tok], expected: u32) {
    assert_eq!(
        opts(toks).unwrap(),
        Options {
            threads: Some(expected)
        }
    );
}

const NUMBER_MESSAGE: &str =
    "give threads a number written as a plain decimal integer, for example `threads = 4`";

#[test]
fn options_empty_stream() {
    assert_eq!(opts(&[]).unwrap(), Options { threads: None });
}

#[test]
fn options_empty_parenthesis_group() {
    assert_eq!(
        opts(&[group(Parenthesis, Vec::new())]).unwrap(),
        Options { threads: None }
    );
}

#[test]
fn options_threads_four() {
    threads_eq(&[id("threads"), punct('='), num("4")], 4);
}

#[test]
fn options_threads_zero() {
    threads_eq(&[id("threads"), punct('='), num("0")], 0);
}

#[test]
fn options_threads_trailing_comma() {
    threads_eq(&[id("threads"), punct('='), num("8"), punct(',')], 8);
}

#[test]
fn options_threads_underscores() {
    threads_eq(&[id("threads"), punct('='), num("1_024")], 1024);
}

#[test]
fn options_threads_max_u32() {
    threads_eq(&[id("threads"), punct('='), num("4294967295")], u32::MAX);
}

#[test]
fn options_error_unknown_option() {
    assert_eq!(
        options_error(&[id("flavor"), punct('='), num("4")]),
        "unknown option `flavor`; #[nuclei::main] accepts only `threads`"
    );
}

#[test]
fn options_error_unknown_option_names_the_macro() {
    let err =
        crate::parse::parse_options(&[id("size"), punct('='), num("4")], Macro::Bench).unwrap_err();
    assert_eq!(
        err.message,
        "unknown option `size`; #[nuclei::bench] accepts only `threads`"
    );
}

#[test]
fn options_error_missing_equals() {
    assert_eq!(
        options_error(&[id("threads"), num("4")]),
        "write the option as `threads = <number>`"
    );
}

#[test]
fn options_error_missing_value() {
    assert_eq!(options_error(&[id("threads"), punct('=')]), NUMBER_MESSAGE);
}

#[test]
fn options_error_boolean_value() {
    assert_eq!(
        options_error(&[id("threads"), punct('='), id("true")]),
        NUMBER_MESSAGE
    );
}

#[test]
fn options_error_negative_value() {
    assert_eq!(
        options_error(&[id("threads"), punct('='), joint('-'), num("1")]),
        NUMBER_MESSAGE
    );
}

#[test]
fn options_error_float_value() {
    assert_eq!(
        options_error(&[id("threads"), punct('='), num("1.5")]),
        NUMBER_MESSAGE
    );
}

#[test]
fn options_error_hex_value() {
    assert_eq!(
        options_error(&[id("threads"), punct('='), num("0x10")]),
        NUMBER_MESSAGE
    );
}

#[test]
fn options_error_suffixed_value() {
    assert_eq!(
        options_error(&[id("threads"), punct('='), num("4u32")]),
        NUMBER_MESSAGE
    );
}

#[test]
fn options_error_group_value() {
    assert_eq!(
        options_error(&[
            id("threads"),
            punct('='),
            group(Parenthesis, vec![num("4")])
        ]),
        NUMBER_MESSAGE
    );
}

#[test]
fn options_error_overflow() {
    assert_eq!(
        options_error(&[id("threads"), punct('='), num("4294967296")]),
        "threads is too large; write a smaller number"
    );
}

#[test]
fn options_error_duplicate() {
    let toks = [
        id("threads"),
        punct('='),
        num("4"),
        punct(','),
        id("threads"),
        punct('='),
        num("8"),
    ];
    assert_eq!(
        options_error(&toks),
        "threads is set more than once; write it a single time"
    );
}

#[test]
fn options_error_leading_comma() {
    assert_eq!(
        options_error(&[punct(','), id("threads"), punct('='), num("4")]),
        "write the option as `threads = <number>`"
    );
}

#[test]
fn options_error_missing_comma_between_options() {
    let toks = [
        id("threads"),
        punct('='),
        num("4"),
        id("threads"),
        punct('='),
        num("8"),
    ];
    assert_eq!(options_error(&toks), "separate options with a comma");
}

#[test]
fn item_minimal() {
    let it = parsed(&item("main", vec![num("1")]));
    assert_eq!(it.name, "main");
    assert!(it.attrs.is_empty());
    assert!(it.vis.is_empty());
    assert!(it.ret.is_empty());
    assert_eq!(render(&it.body), "1");
}

fn ok_unit() -> Vec<Tok> {
    vec![
        id("Ok"),
        group(Parenthesis, vec![group(Parenthesis, Vec::new())]),
    ]
}

#[test]
fn item_return_type_and_body_kept() {
    let it = parsed(&item_ret("main", ret_io_result(), ok_unit()));
    assert_eq!(render(&it.ret), "-> std::io::Result < () >");
    assert_eq!(render(&it.body), "Ok(())");
}

#[test]
fn item_attributes_and_visibility_kept() {
    let attr = vec![
        joint('#'),
        group(
            Bracket,
            vec![id("allow"), group(Parenthesis, vec![id("dead_code")])],
        ),
    ];
    let mut toks = attr.clone();
    toks.push(id("pub"));
    toks.push(group(Parenthesis, vec![id("crate")]));
    toks.extend(item("main", Vec::new()));
    let it = parsed(&toks);
    assert_eq!(render(&it.attrs), "#[allow(dead_code)]");
    assert_eq!(render(&it.vis), "pub(crate)");
}

#[test]
fn item_arbitrary_name_for_test() {
    let it = crate::parse::parse_fn_item(&item("spins", Vec::new()), Macro::Test).unwrap();
    assert_eq!(it.name, "spins");
}

#[test]
fn item_error_plain_fn() {
    let toks = vec![
        id("fn"),
        id("main"),
        group(Parenthesis, Vec::new()),
        group(Brace, Vec::new()),
    ];
    assert_eq!(
        item_error(&toks),
        "the async keyword is missing from the function declaration"
    );
}

#[test]
fn item_error_unsafe_fn() {
    let toks = vec![id("unsafe"), id("async")];
    assert_eq!(
        &item_error(&toks),
        "the async keyword is missing from the function declaration"
    );
}

#[test]
fn item_error_struct() {
    assert_eq!(
        item_error(&[id("struct"), id("S"), punct(';')]),
        "only an async function can be tagged with #[nuclei::main]"
    );
}

#[test]
fn item_error_empty_input() {
    assert_eq!(
        item_error(&[]),
        "only an async function can be tagged with #[nuclei::main]"
    );
}

#[test]
fn item_error_parameters() {
    let toks = vec![
        id("async"),
        id("fn"),
        id("main"),
        group(Parenthesis, vec![id("x"), punct(':'), id("u32")]),
        group(Brace, Vec::new()),
    ];
    assert_eq!(
        item_error(&toks),
        "#[nuclei::main] functions take no parameters"
    );
}

#[test]
fn item_error_generics() {
    let toks = vec![
        id("async"),
        id("fn"),
        id("main"),
        punct('<'),
        id("T"),
        punct('>'),
        group(Parenthesis, Vec::new()),
        group(Brace, Vec::new()),
    ];
    assert_eq!(
        item_error(&toks),
        "generic functions are not supported; write the function without type parameters"
    );
}

#[test]
fn item_error_missing_parameter_list() {
    let toks = vec![id("async"), id("fn"), id("main"), punct(';')];
    assert_eq!(item_error(&toks), "expected the function's parameter list");
}

#[test]
fn item_error_missing_body() {
    let toks = vec![
        id("async"),
        id("fn"),
        id("main"),
        group(Parenthesis, Vec::new()),
    ];
    assert_eq!(item_error(&toks), "expected the function body");
}

#[test]
fn item_error_extra_tokens_after_body() {
    let mut toks = item("main", Vec::new());
    toks.push(id("junk"));
    assert_eq!(
        item_error(&toks),
        "there are extra tokens after the function body; remove them"
    );
}

#[test]
fn item_error_inner_attribute() {
    let toks = vec![
        joint('#'),
        punct('!'),
        group(
            Bracket,
            vec![id("allow"), group(Parenthesis, vec![id("dead_code")])],
        ),
        id("async"),
        id("fn"),
        id("main"),
        group(Parenthesis, Vec::new()),
        group(Brace, Vec::new()),
    ];
    assert_eq!(
        item_error(&toks),
        "inner attributes are not supported here; use outer attributes"
    );
}

#[test]
fn item_error_dangling_hash() {
    assert_eq!(item_error(&[joint('#')]), "expected an attribute after `#`");
}

#[test]
fn main_requires_the_main_name() {
    let it = parsed(&item("start", Vec::new()));
    let err = crate::parse::require_main_name(&it).unwrap_err();
    assert_eq!(
        err.message,
        "only the main function can be tagged with #[nuclei::main]"
    );
}

#[test]
fn expansion_main_default() {
    let body = vec![
        id("println"),
        punct('!'),
        group(Parenthesis, vec![str_lit("hi")]),
        punct(';'),
    ];
    let it = parsed(&item("main", body));
    assert_eq!(
        render(&expand_main(&it, &Options::default())),
        include_str!("../tests/fixtures/main_default.rs").trim()
    );
}

#[test]
fn expansion_main_threads() {
    let it = parsed(&item_ret("main", ret_io_result(), ok_unit()));
    assert_eq!(
        render(&expand_main(&it, &Options { threads: Some(4) })),
        include_str!("../tests/fixtures/main_threads.rs").trim()
    );
}

#[test]
fn expansion_test_default() {
    let attr = [
        joint('#'),
        group(
            Bracket,
            vec![id("allow"), group(Parenthesis, vec![id("dead_code")])],
        ),
    ];
    let body = vec![
        id("assert_eq"),
        punct('!'),
        group(
            Parenthesis,
            vec![num("2"), punct('*'), num("2"), punct(','), num("4")],
        ),
        punct(';'),
    ];
    let mut toks: Vec<Tok> = attr.into();
    toks.extend(item("check", body));
    let it = crate::parse::parse_fn_item(&toks, Macro::Test).unwrap();
    assert_eq!(
        render(&expand_test(&it, &Options::default())),
        include_str!("../tests/fixtures/test_default.rs").trim()
    );
}

#[test]
fn expansion_test_threads() {
    let it = crate::parse::parse_fn_item(&item("spins", vec![num("1")]), Macro::Test).unwrap();
    assert_eq!(
        render(&expand_test(&it, &Options { threads: Some(0) })),
        include_str!("../tests/fixtures/test_threads.rs").trim()
    );
}

#[test]
fn expansion_bench_default() {
    let it =
        crate::parse::parse_fn_item(&item("bench_hello", vec![str_lit("hello")]), Macro::Bench)
            .unwrap();
    assert_eq!(
        render(&expand_bench(&it, &Options::default())),
        include_str!("../tests/fixtures/bench_default.rs").trim()
    );
}

#[test]
fn expansion_bench_threads() {
    let it = crate::parse::parse_fn_item(&item("bench_spawn", Vec::new()), Macro::Bench).unwrap();
    assert_eq!(
        render(&expand_bench(&it, &Options { threads: Some(2) })),
        include_str!("../tests/fixtures/bench_threads.rs").trim()
    );
}

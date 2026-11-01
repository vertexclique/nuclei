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
            threads: Some(expected),
            ..Options::default()
        }
    );
}

const NUMBER_MESSAGE: &str =
    "give threads a number written as a plain decimal integer, for example `threads = 4`";

#[test]
fn options_empty_stream() {
    assert_eq!(opts(&[]).unwrap(), Options::default());
}

#[test]
fn options_empty_parenthesis_group() {
    assert_eq!(
        opts(&[group(Parenthesis, Vec::new())]).unwrap(),
        Options::default()
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
        "unknown option `flavor`; #[nuclei::main] accepts `threads = <number>`, `start_paused = true`, `builder = \"<path>\"` or `crate = \"<path>\"`"
    );
}

#[test]
fn options_error_unknown_option_names_the_macro() {
    let err =
        crate::parse::parse_options(&[id("size"), punct('='), num("4")], Macro::Bench).unwrap_err();
    assert_eq!(
        err.message,
        "unknown option `size`; #[nuclei::bench] accepts `threads = <number>`, `start_paused = true`, `builder = \"<path>\"` or `crate = \"<path>\"`"
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
        "write an option as `threads = <number>`, `start_paused = true`, `builder = \"<path>\"` or `crate = \"<path>\"`"
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
        render(&expand_main(
            &it,
            &Options {
                threads: Some(4),
                ..Options::default()
            }
        )),
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
        render(&expand_test(
            &it,
            &Options {
                threads: Some(0),
                ..Options::default()
            }
        )),
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
        render(&expand_bench(
            &it,
            &Options {
                threads: Some(2),
                ..Options::default()
            }
        )),
        include_str!("../tests/fixtures/bench_threads.rs").trim()
    );
}

#[test]
fn options_start_paused_and_threads() {
    let parsed = opts(&[
        id("start_paused"),
        punct('='),
        id("true"),
        punct(','),
        id("threads"),
        punct('='),
        num("2"),
    ])
    .unwrap();
    assert!(parsed.start_paused);
    assert_eq!(parsed.threads, Some(2));
    assert!(parsed.own_runtime());
    let off = opts(&[id("start_paused"), punct('='), id("false")]).unwrap();
    assert!(!off.start_paused);
    assert!(
        !off.own_runtime(),
        "start_paused = false keeps the default runtime"
    );
}

#[test]
fn options_error_start_paused_needs_a_bool() {
    assert_eq!(
        options_error(&[id("start_paused"), punct('='), num("1")]),
        "give start_paused `true` or `false`"
    );
    assert_eq!(
        options_error(&[id("start_paused"), id("true")]),
        "write the option as `start_paused = true`"
    );
}

#[test]
fn options_error_an_option_twice() {
    assert_eq!(
        options_error(&[
            id("start_paused"),
            punct('='),
            id("true"),
            punct(','),
            id("start_paused"),
            punct('='),
            id("false"),
        ]),
        "start_paused is set more than once; write it a single time"
    );
}

#[test]
fn options_crate_path() {
    let parsed = opts(&[id("crate"), punct('='), str_lit("my_crate::nuclei")]).unwrap();
    assert_eq!(
        parsed.krate,
        Some(vec!["my_crate".to_string(), "nuclei".to_string()])
    );
    let rooted = opts(&[id("crate"), punct('='), str_lit("::rt")]).unwrap();
    assert_eq!(rooted.krate, Some(vec!["rt".to_string()]));
}

#[test]
fn options_error_crate_path() {
    for bad in ["", "a::", "1x", "a b", "a::::b"] {
        let message = options_error(&[id("crate"), punct('='), str_lit(bad)]);
        assert!(
            message.starts_with("give crate the path"),
            "{bad:?}: {message}"
        );
    }
    let message = options_error(&[id("crate"), punct('='), id("nuclei")]);
    assert!(message.starts_with("give crate the path"), "{message}");
}

#[test]
fn expand_test_start_paused_builds_a_paused_runtime_of_its_own() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        start_paused: true,
        ..Options::default()
    };
    let out = render(&expand_test(&it, &opts));
    assert!(
        out.contains("::nuclei::Runtime::builder().worker_threads(0).start_paused(true).build()"),
        "{out}"
    );
}

#[test]
fn expand_main_uses_the_crate_path() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        krate: Some(vec!["my_crate".to_string(), "nuclei".to_string()]),
        ..Options::default()
    };
    let out = render(&expand_main(&it, &opts));
    assert!(out.contains("::my_crate::nuclei::block_on(async"), "{out}");
    assert!(
        !out.contains("{ ::nuclei::"),
        "the default path is gone: {out}"
    );
}

#[test]
fn expand_main_keeps_a_relative_crate_path() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        krate: Some(vec!["crate".to_string(), "rt".to_string()]),
        ..Options::default()
    };
    let out = render(&expand_main(&it, &opts));
    assert!(out.contains("{ crate::rt::block_on(async"), "{out}");
}

const BUILDER_MESSAGE: &str = "give builder the path of a function that returns a nuclei::Builder, as a string: `builder = \"runtime::builder\"`";

fn builder_option(path: &str) -> Vec<Tok> {
    vec![id("builder"), punct('='), str_lit(path)]
}

fn builder_of(path: &str) -> Option<Vec<String>> {
    opts(&builder_option(path)).unwrap().builder
}

fn segments(parts: &[&str]) -> Option<Vec<String>> {
    Some(parts.iter().map(|part| part.to_string()).collect())
}

#[test]
fn options_builder_path() {
    assert_eq!(
        builder_of("runtime::builder"),
        segments(&["runtime", "builder"])
    );
    assert_eq!(
        builder_of("::runtime::builder"),
        segments(&["runtime", "builder"])
    );
    assert_eq!(builder_of("build"), segments(&["build"]));
    assert_eq!(
        builder_of("crate::cfg::_build2"),
        segments(&["crate", "cfg", "_build2"])
    );
    assert_eq!(builder_of("self::build"), segments(&["self", "build"]));
}

#[test]
fn options_builder_sets_nothing_else() {
    assert_eq!(
        opts(&builder_option("a::b")).unwrap(),
        Options {
            builder: segments(&["a", "b"]),
            ..Options::default()
        }
    );
    assert!(opts(&builder_option("a::b")).unwrap().own_runtime());
}

#[test]
fn options_builder_beside_the_others_in_any_order() {
    let mut toks = builder_option("cfg::build");
    toks.extend([punct(','), id("threads"), punct('='), num("2"), punct(',')]);
    toks.extend([id("start_paused"), punct('='), id("true"), punct(',')]);
    toks.extend([
        id("crate"),
        punct('='),
        str_lit("my_crate::nuclei"),
        punct(','),
    ]);
    let parsed = opts(&toks).unwrap();
    assert_eq!(
        parsed,
        Options {
            threads: Some(2),
            start_paused: true,
            krate: segments(&["my_crate", "nuclei"]),
            builder: segments(&["cfg", "build"]),
        }
    );
    let mut reversed = vec![id("threads"), punct('='), num("2"), punct(',')];
    reversed.extend(builder_option("cfg::build"));
    reversed.push(punct(','));
    assert_eq!(
        opts(&reversed).unwrap().builder,
        segments(&["cfg", "build"])
    );
}

#[test]
fn options_builder_is_accepted_by_every_macro() {
    for mac in [Macro::Main, Macro::Test, Macro::Bench] {
        let parsed = crate::parse::parse_options(&builder_option("a::b"), mac).unwrap();
        assert_eq!(parsed.builder, segments(&["a", "b"]));
    }
}

#[test]
fn options_error_builder_twice() {
    let mut toks = builder_option("a::b");
    toks.push(punct(','));
    toks.extend(builder_option("c::d"));
    assert_eq!(
        options_error(&toks),
        "builder is set more than once; write it a single time"
    );
}

#[test]
fn options_error_builder_value() {
    for bad in ["", "::", "a::", "1x", "a b", "a::::b", "a-b", "a::b()"] {
        assert_eq!(
            options_error(&builder_option(bad)),
            BUILDER_MESSAGE,
            "{bad:?}"
        );
    }
    let wrong_kinds = [
        id("runtime"),
        num("4"),
        id("true"),
        Tok::Literal("r\"a::b\"".to_string(), None),
        group(Parenthesis, vec![str_lit("a::b")]),
    ];
    for value in wrong_kinds {
        assert_eq!(
            options_error(&[id("builder"), punct('='), value.clone()]),
            BUILDER_MESSAGE,
            "{value:?}"
        );
    }
    assert_eq!(options_error(&[id("builder"), punct('=')]), BUILDER_MESSAGE);
}

#[test]
fn options_error_builder_missing_equals() {
    assert_eq!(
        options_error(&[id("builder"), str_lit("a::b")]),
        "write the option as `builder = \"<path>\"`"
    );
}

#[test]
fn options_error_builder_not_followed_by_a_comma() {
    let mut toks = builder_option("a::b");
    toks.extend([id("threads"), punct('='), num("2")]);
    assert_eq!(options_error(&toks), "separate options with a comma");
}

#[test]
fn expand_test_builds_the_runtime_from_the_builder_function() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        builder: segments(&["runtime", "builder"]),
        ..Options::default()
    };
    let out = render(&expand_test(&it, &opts));
    assert!(
        out.contains(
            "runtime::builder().build().expect(\"the nuclei runtime failed to start\").block_on(async"
        ),
        "{out}"
    );
    assert!(!out.contains("worker_threads"), "{out}");
    assert!(!out.contains("start_paused"), "{out}");
    assert!(
        !out.contains("::nuclei::block_on"),
        "the default runtime is used: {out}"
    );
    assert!(!out.contains("Runtime::builder"), "{out}");
}

#[test]
fn expand_main_applies_threads_on_top_of_the_builder() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        threads: Some(3),
        builder: segments(&["crate", "cfg", "build"]),
        ..Options::default()
    };
    let out = render(&expand_main(&it, &opts));
    assert!(
        out.contains("crate::cfg::build().worker_threads(3).build().expect("),
        "{out}"
    );
}

#[test]
fn expand_test_applies_start_paused_on_top_of_the_builder() {
    let it = parsed(&item("main", Vec::new()));
    let paused = Options {
        start_paused: true,
        builder: segments(&["build"]),
        ..Options::default()
    };
    let out = render(&expand_test(&it, &paused));
    assert!(
        out.contains("build().start_paused(true).build().expect("),
        "{out}"
    );
    assert!(
        !out.contains("worker_threads"),
        "a paused builder keeps its workers: {out}"
    );

    let both = Options {
        threads: Some(2),
        start_paused: true,
        builder: segments(&["build"]),
        ..Options::default()
    };
    let out = render(&expand_test(&it, &both));
    assert!(
        out.contains("build().worker_threads(2).start_paused(true).build().expect("),
        "{out}"
    );
}

#[test]
fn expand_test_with_threads_zero_and_a_builder_asks_for_no_workers() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        threads: Some(0),
        builder: segments(&["build"]),
        ..Options::default()
    };
    let out = render(&expand_test(&it, &opts));
    assert!(out.contains("build().worker_threads(0).build()"), "{out}");
}

#[test]
fn expand_names_the_builder_function_as_written_beside_a_crate_path() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        krate: segments(&["my_crate", "nuclei"]),
        builder: segments(&["cfg", "build"]),
        ..Options::default()
    };
    let out = render(&expand_main(&it, &opts));
    assert!(out.contains("{ cfg::build().build().expect("), "{out}");
    assert!(
        !out.contains("my_crate"),
        "the builder is not under the crate path: {out}"
    );
}

#[test]
fn expand_bench_builds_its_runtime_once_from_the_builder_function() {
    let it = parsed(&item("main", Vec::new()));
    let opts = Options {
        builder: segments(&["runtime", "builder"]),
        ..Options::default()
    };
    let out = render(&expand_bench(&it, &opts));
    assert!(
        out.contains(
            "let __nuclei_runtime = runtime::builder().build().expect(\"the nuclei runtime failed to start\");"
        ),
        "{out}"
    );
    assert!(out.contains("__nuclei_runtime.block_on(async"), "{out}");
}

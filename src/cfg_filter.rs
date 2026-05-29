//! Compile-time `@cfg(...)` item filtering — feature #054.
//!
//! `@cfg(arch="x86_64")` attached to the item that immediately follows it
//! makes that item conditional on the compile target.  Items whose `@cfg`
//! arch does not match the target are dropped before registry population,
//! resolve, typeck and codegen ever see them — so two `@cfg` consts with
//! the same name never collide.
//!
//! Surface form (the directive is a standalone `Item::Directive` that the
//! parser leaves immediately before the item it annotates):
//!
//! ```text
//! @cfg(arch="x86_64")
//! pub const SYS_SOCKET = 41
//!
//! @cfg(arch="aarch64")
//! pub const SYS_SOCKET = 198
//! ```
//!
//! Both declare `SYS_SOCKET`; exactly one survives per target.  An item
//! without a preceding `@cfg` is unconditional and kept for all targets.
//!
//! See docs/state/features/054_cfg.md for the full design + rationale.

use chumsky::span::Spanned;

use crate::api::ast::{Directive, Item};
use crate::error_handle::LakeError;
use crate::loader::ParsedProgram;

/// The compile target arch a `@cfg(arch="...")` predicate is matched
/// against.  Threaded in from the backend's `--target` flag (defaulting
/// to the compiler host) so stdlib can declare per-arch items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CfgTarget<'a> {
    pub arch: &'a str,
}

impl<'a> CfgTarget<'a> {
    pub fn new(arch: &'a str) -> Self {
        Self { arch }
    }
}

/// Filter every module of a parsed program against `target`.  Drops items
/// whose preceding `@cfg(arch=X)` does not match, along with the `@cfg`
/// directive items themselves (matched or not — they carry no codegen
/// meaning).  Returns the surviving program plus any malformed-`@cfg`
/// diagnostics (E041).
pub fn filter_program<'src>(
    mut program: ParsedProgram<'src>,
    target: CfgTarget<'_>,
) -> (ParsedProgram<'src>, Vec<LakeError>) {
    let mut errors = Vec::new();
    for module in &mut program.modules {
        let ast = std::mem::take(&mut module.ast);
        let (kept, errs) = filter_items(ast, target);
        module.ast = kept;
        for mut e in errs {
            if e.source_path.is_none() {
                e.source_path = Some(module.source_path.clone());
            }
            errors.push(e);
        }
    }
    (program, errors)
}

/// Filter a single module's flat item list.  A `@cfg` directive applies to
/// the item that immediately follows it; the directive item itself is always
/// dropped.  A `@cfg` at end-of-file (no following item) is reported as a
/// diagnostic and otherwise ignored.
pub fn filter_items<'src>(
    items: Vec<Spanned<Item<'src>>>,
    target: CfgTarget<'_>,
) -> (Vec<Spanned<Item<'src>>>, Vec<LakeError>) {
    let mut out = Vec::with_capacity(items.len());
    let mut errors = Vec::new();
    let mut pending: Option<Spanned<Item<'src>>> = None;

    for item in items {
        // A `@cfg` directive becomes the pending predicate for the next item.
        if let Item::Directive(d) = &item.inner {
            if d.inner.name.inner.0 == "cfg" {
                if let Some(prev) = pending.take() {
                    // Two `@cfg` in a row — the earlier one had no item to
                    // attach to.  Report and drop it.
                    errors.push(dangling_cfg(&prev));
                }
                pending = Some(item);
                continue;
            }
        }

        match pending.take() {
            Some(cfg_item) => {
                let Item::Directive(d) = &cfg_item.inner else {
                    unreachable!("pending is only ever a @cfg directive");
                };
                match eval_cfg(&d.inner, target.arch) {
                    Ok(true) => out.push(item),
                    Ok(false) => {
                        // Well-formed predicate, arch mismatched — drop both
                        // the directive and the item it annotates.
                    }
                    Err(e) => {
                        // Malformed `@cfg`: surface the error and keep the
                        // item so later passes still see a (probably also
                        // broken) program rather than silently vanishing it.
                        errors.push(e);
                        out.push(item);
                    }
                }
            }
            None => out.push(item),
        }
    }

    if let Some(prev) = pending {
        errors.push(dangling_cfg(&prev));
    }

    (out, errors)
}

/// Evaluate a `@cfg(arch="...")` predicate against `target`.  Returns
/// `Ok(true)` to keep the annotated item, `Ok(false)` to drop it (arch
/// mismatch), or `Err` on malformed input (no key, or an unknown key).
///
/// Multiple `arch="..."` values are OR'd: `@cfg(arch="x86_64" arch="aarch64")`
/// keeps the item on either arch.  Today stdlib only uses a single value.
fn eval_cfg(d: &Directive<'_>, target: &str) -> Result<bool, LakeError> {
    if d.kv_args.is_empty() {
        return Err(malformed(
            d,
            "`@cfg` requires a key, e.g. `@cfg(arch=\"x86_64\")`",
        ));
    }
    for (key, _) in &d.kv_args {
        if key.inner.0 != "arch" {
            return Err(unknown_key(key));
        }
    }
    Ok(d.kv_args.iter().any(|(_, v)| v.inner == target))
}

fn malformed(d: &Directive<'_>, msg: &str) -> LakeError {
    LakeError::new(format!("malformed `@cfg`: {msg}"), d.name.span)
        .code("E041")
        .help("use `@cfg(arch=\"x86_64\")` or `@cfg(arch=\"aarch64\")`")
}

fn unknown_key(key: &Spanned<crate::api::ast::Ident<'_>>) -> LakeError {
    LakeError::new(
        format!("unknown `@cfg` key `{}`", key.inner.0),
        key.span,
    )
    .code("E041")
    .help("the only supported `@cfg` key is `arch`")
}

fn dangling_cfg(item: &Spanned<Item<'_>>) -> LakeError {
    LakeError::new(
        "`@cfg` is not attached to an item".to_string(),
        item.span,
    )
    .code("E041")
    .help("place `@cfg(arch=\"...\")` immediately before a const or machine")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lexer;
    use crate::parser::program;
    use chumsky::input::Input;
    use chumsky::{Parser, span::Spanned};

    fn parse(src: &str) -> Vec<Spanned<Item<'_>>> {
        let tokens = lexer().parse(src).into_result().expect("lex");
        program()
            .parse(tokens[..].split_spanned((0..src.len()).into()))
            .into_result()
            .expect("parse")
    }

    fn count_consts(items: &[Spanned<Item<'_>>]) -> usize {
        items
            .iter()
            .filter(|i| matches!(i.inner, Item::Const(_)))
            .count()
    }

    #[test]
    fn cfg_arch_parses_and_attaches() {
        let items = parse("@cfg(arch=\"x86_64\")\npub const X = 1");
        // Two items pre-filter: the directive + the const.
        assert_eq!(items.len(), 2);
        let Item::Directive(d) = &items[0].inner else {
            panic!("expected directive, got {:?}", items[0].inner)
        };
        assert_eq!(d.inner.name.inner.0, "cfg");
        assert_eq!(d.inner.kv_args.len(), 1);
        assert_eq!(d.inner.kv_args[0].0.inner.0, "arch");
        assert_eq!(d.inner.kv_args[0].1.inner, "x86_64");
    }

    #[test]
    fn filter_keeps_matching_arch() {
        let items = parse("@cfg(arch=\"x86_64\")\npub const X = 1");
        let (kept, errs) = filter_items(items, CfgTarget::new("x86_64"));
        assert!(errs.is_empty(), "errs: {errs:?}");
        // Directive dropped, const kept.
        assert_eq!(kept.len(), 1);
        assert_eq!(count_consts(&kept), 1);
    }

    #[test]
    fn filter_drops_mismatched_arch() {
        let items = parse("@cfg(arch=\"x86_64\")\npub const X = 1");
        let (kept, errs) = filter_items(items, CfgTarget::new("aarch64"));
        assert!(errs.is_empty(), "errs: {errs:?}");
        // Both directive AND const dropped.
        assert!(kept.is_empty(), "expected empty, got {kept:?}");
    }

    #[test]
    fn two_cfg_same_name_resolves_to_one_per_target() {
        let src = "@cfg(arch=\"x86_64\")\npub const SYS_SOCKET = 41\n\
                   @cfg(arch=\"aarch64\")\npub const SYS_SOCKET = 198";
        // x86_64: keep the 41 one.
        let (kept, errs) = filter_items(parse(src), CfgTarget::new("x86_64"));
        assert!(errs.is_empty());
        assert_eq!(count_consts(&kept), 1);
        let Item::Const(c) = &kept[0].inner else { panic!() };
        assert!(
            matches!(&c.inner.value.inner, crate::api::expr::Expr::Num("41", _)),
            "x86_64 should keep SYS_SOCKET=41, got {:?}",
            c.inner.value.inner
        );

        // aarch64: keep the 198 one.
        let (kept, errs) = filter_items(parse(src), CfgTarget::new("aarch64"));
        assert!(errs.is_empty());
        assert_eq!(count_consts(&kept), 1);
        let Item::Const(c) = &kept[0].inner else { panic!() };
        assert!(
            matches!(&c.inner.value.inner, crate::api::expr::Expr::Num("198", _)),
            "aarch64 should keep SYS_SOCKET=198, got {:?}",
            c.inner.value.inner
        );
    }

    #[test]
    fn unconditional_item_kept_for_all_targets() {
        let items = parse("pub const X = 1");
        let (kept_x, _) = filter_items(parse("pub const X = 1"), CfgTarget::new("x86_64"));
        let (kept_a, _) = filter_items(items, CfgTarget::new("aarch64"));
        assert_eq!(count_consts(&kept_x), 1);
        assert_eq!(count_consts(&kept_a), 1);
    }

    #[test]
    fn cfg_attaches_to_machine() {
        let src = "@cfg(arch=\"aarch64\")\npub m is { _ -> { } }";
        let (kept, errs) = filter_items(parse(src), CfgTarget::new("x86_64"));
        assert!(errs.is_empty());
        assert!(kept.is_empty(), "machine should drop on mismatch");
        let (kept, _) = filter_items(parse(src), CfgTarget::new("aarch64"));
        assert_eq!(kept.len(), 1);
        assert!(matches!(kept[0].inner, Item::Machine(_)));
    }

    #[test]
    fn unknown_cfg_key_errors() {
        let items = parse("@cfg(os=\"linux\")\npub const X = 1");
        let (_, errs) = filter_items(items, CfgTarget::new("x86_64"));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code.as_deref(), Some("E041"));
    }

    #[test]
    fn dangling_cfg_at_eof_errors() {
        let items = parse("@cfg(arch=\"x86_64\")");
        let (_, errs) = filter_items(items, CfgTarget::new("x86_64"));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code.as_deref(), Some("E041"));
    }
}

//! Built-in runtime function signatures.
//!
//! Lake currently follows **Model A** for runtime functions: the frontend is
//! the single source of truth for what `@rt(...)` names exist and how they
//! are typed.  Each Lake backend (currently only the native Cranelift one in
//! `lake-native-compiler`) is contractually obliged to implement these exact
//! signatures.
//!
//! When a second backend appears (e.g. WASM) and its rt surface diverges
//! enough that listing it here is awkward, this module is the obvious place
//! to split — the *contract list* moves into a shared `lake-rt-core` crate
//! and per-backend extensions register additional sigs through the
//! [`crate::registry::ProgramRegistry::extend_rt`] hook.  No callers need to
//! change.

use crate::registry::Signature;

/// Build a signature from string slices.  Helper for the static table
/// below — keeps the table itself terse.
fn s(params: &[&'static str], ret: &'static str) -> Signature {
    Signature {
        params: params.iter().map(|p| (*p).to_string()).collect(),
        ret: ret.to_string(),
    }
}

/// Returns the static set of built-in runtime function signatures.
///
/// Adding a new rt fn:
///   1. Implement it in `lake-native-compiler/src/compiler/rt/funcs/`.
///   2. Add a row here with the same parameter and return types.
///   3. Programs that want to call it write `@rt(name)` — the frontend will
///      look up the signature here for type checking.
///
/// The map values are `Signature`s; ownership lets callers freely embed them
/// in [`crate::registry::ProgramRegistry`] without any further allocations.
pub fn builtin_signatures() -> Vec<(&'static str, Signature)> {
    vec![
        // ── stdio + raw bytes ────────────────────────────────────────────
        ("rt_write", s(&["i64", "buf", "i64"], "{}")),
        ("rt_write_static", s(&["i64", "str", "i64"], "{}")),
        ("rt_read", s(&["i64", "buf", "i64"], "{}")),

        // ── memory ──────────────────────────────────────────────────────
        // rt_allocate returns the tuple `{atom buf}` (tuple-ABI #87):
        // user code pattern-matches `r.0` against `:ok` / `:err`
        // before consuming `r.1`.  rt_allocate_raw stays plain buf
        // for scheduler internals.
        ("rt_allocate", s(&["i64"], "{atom buf}")),
        ("rt_allocate_raw", s(&["i64"], "buf")),
        // #138 per-actor arena bump allocator.  Plain `buf` ABI matching
        // rt_allocate_raw (its fallback when the arena is exhausted /
        // absent).  Caller never gets `{atom buf}` failure tuples — if
        // the bucket fallback also OOMs, the rt-fn dies the actor and
        // doesn't return.  Wrapped by stdlib `alloc` / `alloc_or_die`
        // helpers that present the tuple-ABI surface.
        ("rt_arena_alloc", s(&["i64"], "buf")),
        ("rt_die_actor", s(&[], "{}")),
        ("rt_free", s(&["buf"], "{}")),
        ("rt_store", s(&["buf", "i64", "i64", "i64"], "{}")),
        ("rt_load_u8", s(&["buf", "i64"], "i64")),
        ("rt_load_u16", s(&["buf", "i64"], "i64")),
        ("rt_load_u32", s(&["buf", "i64"], "i64")),
        ("rt_load_u64", s(&["buf", "i64"], "i64")),
        ("rt_copy_bytes", s(&["buf", "i64", "buf", "i64", "i64"], "{}")),
        ("rt_mmap", s(&["i64", "i64", "i64", "i64", "i64", "i64"], "i64")),
        ("rt_munmap", s(&["i64", "i64"], "i64")),
        ("rt_init_heap", s(&[], "{}")),

        // ── string helpers ──────────────────────────────────────────────
        //
        // At the runtime level these all traffic in fat-pointer addresses
        // (i64), but at the source level Lake types them as `str` so user
        // code reads naturally:
        //   `let s = to_string(n)` → s : str
        //   `len(s)`               → fine, matches the str param
        ("len", s(&["str"], "i64")),
        ("rt_str_ptr", s(&["str"], "i64")),
        ("to_string", s(&["i64"], "str")),
        ("to_string_with_ln", s(&["i64"], "str")),

        // ── process / scheduler ─────────────────────────────────────────
        ("rt_exit", s(&["i64"], "{}")),
        ("rt_syscall", s(
            &["i64", "i64", "i64", "i64", "i64", "i64", "i64"],
            "i64",
        )),

        // ── async I/O ───────────────────────────────────────────────────
        ("rt_io_uring_setup", s(&[], "{}")),
        ("rt_io_uring_flush", s(&[], "{}")),
        ("rt_io_uring_poll_cq", s(&[], "{}")),
        ("rt_io_uring_wait_cqe", s(&[], "{}")),
        ("rt_io_park_current", s(&[], "{}")),
        ("rt_write_async", s(&["i64", "str", "i64"], "{}")),

        // ── child process (pidfd + io_uring POLLADD) — #099 ───────────
        ("rt_clone3_pidfd", s(&["i64"], "i64")),
        ("rt_pidfd_poll_async", s(&["i64"], "i64")),
        ("rt_waitid_pidfd", s(&["i64"], "i64")),

        // ── network ────────────────────────────────────────────────────
        ("rt_listen_tcp", s(&["i64"], "i64")),
        ("rt_accept_async", s(&["i64"], "i64")),
        ("rt_send_async", s(&["i64", "str", "i64"], "i64")),
        ("rt_recv_async", s(&["i64", "buf", "i64"], "i64")),
        ("rt_close", s(&["i64"], "{}")),

        // ── environment (argv / envp via asm shim) ─────────────────────
        ("rt_argc_raw", s(&[], "i64")),
        ("rt_argv_raw", s(&[], "i64")),
        ("rt_envp_raw", s(&[], "i64")),
        ("rt_cstr_len", s(&["i64"], "i64")),
        ("rt_load_ptr_raw", s(&["i64", "i64"], "i64")),
        ("rt_cstr_to_buf", s(&["i64"], "{atom buf}")),
        ("buf_len", s(&["buf"], "i64")),
        ("buf_ptr", s(&["buf"], "i64")),
        ("cmp_str_buf", s(&["str", "buf"], "i64")),
        ("buf_trim", s(&["buf", "i64"], "{}")),
    ]
}

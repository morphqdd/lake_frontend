# Lake Error Codes

Errors are emitted by the three compiler passes in order: **parse** → **resolve** → **typecheck**.
Each diagnostic includes its code, a one-line description, an example that triggers it, and a suggested fix.

---

## Parse errors (no code)

Parse errors come directly from the chumsky parser and do not carry a Lake error code.
They are displayed with full source context via ariadne.

---

## Type-check errors

### E001 — Undeclared variable

A variable name appears in a branch body but was never declared in the branch parameters or a `let` binding.

```lake
counter is {
    _ -> {
        x          -- E001: undeclared variable `x`
    }
}
```

**Fix:** add the variable to the branch parameters with its type:

```lake
counter is {
    x i64 -> {
        x          -- ok
    }
}
```

---

### E002 — Arithmetic on a value of unknown type

One or both operands of `+`, `-`, `*`, or `/` still have an unresolved `{}` type after the resolver pass, meaning the operand is undeclared.

```lake
counter is {
    _ -> {
        x + 1      -- E002: arithmetic on a value of unknown type
    }
}
```

**Fix:** declare `x` in the branch parameters:

```lake
counter is {
    x i64 -> {
        x + 1      -- ok
    }
}
```

---

### E003 — `self(…)` with no matching branch

A `self(args)` call does not match any branch of the enclosing machine.
Matching is based on the number and types of the arguments.

```lake
counter is {
    n i64 -> {
        self()     -- E003: no branch of `counter` accepts ()
    }
}
```

**Fix:** either add a branch with the right signature, or pass arguments that match an existing branch:

```lake
counter is {
    n i64 -> {
        self(n)    -- ok — matches `n i64 -> …`
    }
}
```

# Lake Error Codes

Errors are emitted by the four compiler passes in order: **lex → parse → resolve → typecheck**.
Each diagnostic includes its code, a one-line description, an example that triggers it, and a suggested fix.

---

## Lex errors

### L001 — Unexpected character

An unexpected character was encountered during lexing.

```lake
counter is { n i64 -> { n + @n } }
--                          ^ L001: unexpected character
```

**Fix:** remove or replace the unexpected character.

---

## Parse errors

### P001 — Unexpected token

A token appeared in a position where the parser did not expect it.

```lake
counter is { n i64 -> n }
--                    ^ P001: expected `{`
```

**Fix:** correct the syntax.  Refer to the Lake language reference for valid constructs.

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

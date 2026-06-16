# Rill CLI — Design

## Overview

A command-line interpreter for Rill scripts, suitable for `#!/usr/bin/env rill`
shebang execution. Bundles a standard library of useful externs and calls
`fn main(args)` in the input file.

Separate crate (`rill-cli`) depending on the `rill` library — keeps the core
language free of I/O dependencies.

## Commands

Two separate binaries:

- **`rill`** — run a script. Single command, no subcommands. Shebang-compatible.
- **`rillc`** — compiler toolchain with subcommands (future, separate task).

### `rill` — Script Runner

```bash
rill script.rill                      # run main(), no args
rill script.rill arg1 arg2            # run main([arg1, arg2])
```

Shebang (portable — no subcommand needed):
```rill
#!/usr/bin/env rill
fn main(args) {
    println("hello " + args[0]);
}
```

### `rillc` — Compiler Toolchain (future)

```bash
rillc check script.rill               # parse + optimize, report diagnostics
rillc dump script.rill [function]     # dump optimized IR
rillc build script.rill -o out.rillc  # compile to bytecode
```

`rillc` is a separate binary with subcommands via clap. Designed separately.

## Entry Point

The CLI looks for `fn main(args)` in the root source file.

| Signature | Behavior |
|---|---|
| `fn main(args)` | Called with Array of Text (command-line arguments) |
| `fn main()` | Called with zero arguments |
| No `main` | Error: "no main function found" |

**Exit code:**
- `main` returns UInt → process exit code
- `main` returns other defined value → exit 0
- `main` returns Undefined → exit 1
- `exit(code)` extern → immediate exit with code (via `ExecResult::Exit`)

## Standard Library

The CLI registers a standard set of externs before compilation. These are
only available to scripts run via the CLI — library embedders register their
own externs.

### I/O (`require io;`)

| Function | Signature | Description |
|---|---|---|
| `print(value)` | any → Undefined | Print value to stdout, no newline |
| `println(value)` | any → Undefined | Print value to stdout with newline |
| `eprint(value)` | any → Undefined | Print to stderr, no newline |
| `eprintln(value)` | any → Undefined | Print to stderr with newline |
| `read_line()` | → Text or Undefined | Read line from stdin (Undefined on EOF) |
| `read_file(path)` | Text → Text or Undefined | Read entire file (Undefined on error) |
| `write_file(path, data)` | Text, Text/Bytes → Bool | Write file, returns success |

### Process (`require process;`)

| Function | Signature | Description |
|---|---|---|
| `exit(code)` | UInt → diverges | Exit immediately with code |
| `env(name)` | Text → Text or Undefined | Read environment variable |

### String (`require str;`)

| Function | Signature | Description |
|---|---|---|
| `upper(s)` | Text → Text | Uppercase |
| `lower(s)` | Text → Text | Lowercase |
| `trim(s)` | Text → Text | Strip whitespace |
| `split(s, delim)` | Text, Text → Array | Split into array of Text |
| `join(arr, delim)` | Array, Text → Text | Join array with delimiter |
| `contains(s, sub)` | Text, Text → Bool | Substring test |
| `replace(s, from, to)` | Text, Text, Text → Text | Replace all occurrences |
| `starts_with(s, prefix)` | Text, Text → Bool | Prefix test |
| `ends_with(s, suffix)` | Text, Text → Bool | Suffix test |
| `substr(s, start, len)` | Text, UInt, UInt → Text | Substring extraction |
| `char_at(s, i)` | Text, UInt → UInt | Character code at index |
| `from_char(code)` | UInt → Text | Character from code point |

### Math (`require math;`)

| Function | Signature | Description |
|---|---|---|
| `floor(x)` | Float → Int | Round toward negative infinity |
| `ceil(x)` | Float → Int | Round toward positive infinity |
| `round(x)` | Float → Int | Round to nearest |
| `trunc(x)` | Float → Int | Round toward zero |
| `sqrt(x)` | numeric → Float | Square root |
| `pow(base, exp)` | numeric, numeric → numeric | Exponentiation |
| `log(x)` | Float → Float | Natural logarithm |
| `abs(x)` | numeric → numeric | Absolute value |
| `min(a, b)` | numeric, numeric → numeric | Minimum |
| `max(a, b)` | numeric, numeric → numeric | Maximum |
| `sin(x)` | Float → Float | Sine |
| `cos(x)` | Float → Float | Cosine |

### Encoding (`require encoding;`)

| Function | Signature | Description |
|---|---|---|
| `hex_encode(data)` | Bytes → Text | Hex encode |
| `hex_decode(s)` | Text → Bytes or Undefined | Hex decode |
| `base64_encode(data)` | Bytes → Text | Base64 encode |
| `base64_decode(s)` | Text → Bytes or Undefined | Base64 decode |

### Format (`require fmt;`)

| Function | Signature | Description |
|---|---|---|
| `to_text(value)` | any → Text | Convert any value to text representation |
| `format(template, ..args)` | Text, ..any → Text | String interpolation with `{}` placeholders |

## Standard Prelude

The CLI auto-imports a standard prelude via the SourceLoader before compiling
the user's script. The prelude provides utility functions written in Rill:

```rill
// prelude.rill — auto-imported by CLI
fn is_defined(x) { if let _ = x { true } else { false } }
fn is_uint(x) { match x { UInt(_) => true, _ => false } }
fn is_int(x) { match x { Int(_) => true, _ => false } }
fn is_float(x) { match x { Float(_) => true, _ => false } }
fn is_text(x) { match x { Text(_) => true, _ => false } }
fn is_bytes(x) { match x { Bytes(_) => true, _ => false } }
fn is_array(x) { match x { Array(_) => true, _ => false } }
fn is_map(x) { match x { Map(_) => true, _ => false } }
fn is_bool(x) { match x { Bool(_) => true, _ => false } }
fn default(value, fallback) { if let v = value { v } else { fallback } }
fn clamp(x, lo, hi) { if x < lo { lo } else if x > hi { hi } else { x } }
```

The prelude is merged into root scope (`as _`) — functions are available
unqualified. Library embedders can provide their own prelude or none at all.

## Project Structure

Lives in `tools/rill/` following the existing workspace pattern (like
`tools/ping`, `tests/interop/mtcp/` in Hardy):

```
tools/rill/
  Cargo.toml          — depends on rill crate, minimal other deps
  src/
    main.rs            — entry point, arg parsing, file loading, dispatch
    stdlib/
      mod.rs           — register_stdlib(registry) → ExternRegistry
      io.rs            — print, println, read_line, read_file, write_file
      process.rs       — exit, env
      str.rs           — string operations
      math.rs          — numeric functions
      encoding.rs      — hex, base64
      fmt.rs           — to_text, format
  prelude.rill         — standard prelude source (embedded via include_str!)
```

## Implementation

### Script Execution Flow

```rust
fn run_script(path: &str, args: &[String]) -> Result<i32, Error> {
    // 1. Set up externs
    let mut externs = rill::standard_externs();
    stdlib::register_stdlib(&mut externs);

    // 2. Compile with prelude
    let loader = FileLoader::new(script_dir);
    let mut compiler = Compiler::with_externs(externs, &loader);
    compiler.add_source("prelude.rill", PRELUDE_SOURCE);  // built-in
    compiler.add(path)?;
    let program = compiler.build()?;

    // 3. Initialize globals and execute
    let mut vm = VM::new();
    vm.exec(&program)?;  // reserves global slots, runs __init__

    // 4. Call main
    let args_value = args_to_array(args);
    vm.push(args_value)?;

    match program.call(&mut vm, "main", 1) {
        Ok(Value::UInt(code)) => Ok(code as i32),
        Ok(_) => Ok(0),
        Err(ExecError::Exit(Value::UInt(code))) => Ok(code as i32),
        Err(e) => Err(e.into()),
    }
}
```


## Error Handling

| Error | Exit code | Output |
|---|---|---|
| File not found | 2 | `error: cannot open 'script.rill'` |
| Parse error | 2 | Diagnostic with source context |
| Compile error | 2 | Diagnostic with source context |
| Runtime error (StackOverflow, HeapOverflow) | 1 | `runtime error: stack overflow` |
| `exit(N)` | N | (none) |
| `main` returns Undefined | 1 | (none) |
| `main` returns UInt(N) | N | (none) |
| `main` returns other | 0 | (none) |

## Future Extensions

- **`rillc`**: compiler toolchain — `check`, `dump`, `build` (bytecode), separate design
- **REPL**: `rill` with no arguments → runs a built-in REPL written in Rill
  itself (using `io::read_line()`, `io::println()` from the stdlib). No special
  REPL mode in the binary — just a Rill script that reads, evaluates, and prints.
- **Shebang caching**: cache compiled bytecode alongside script (`.rill.cache`)
  for faster re-execution when source hasn't changed
- **Debugger**: `rill --debug script.rill` — step through execution
- **Profiler**: `rill --profile script.rill` — execution timing per function

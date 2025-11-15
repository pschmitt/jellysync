# Development Guidelines for jellysync

## Bash Scripting Rules

### Error Handling

**DO NOT** use `set -e` or `set -euo pipefail`. Instead, handle errors explicitly:

```bash
# ✗ WRONG
set -e
result=$(command)

# ✗ WRONG
command || true

# ✓ CORRECT
if ! result=$(command 2>/dev/null)
then
  echo "ERROR: command failed"
  return 1
fi
```

**DO NOT** create wrapper functions like `die()`. Log the error and return explicitly:

```bash
# ✗ WRONG - wrapper function
die() {
  log_error "$@"
  return 1
}

if [[ error ]]
then
  die "Something failed"
fi

# ✓ CORRECT - explicit error handling
if [[ error ]]
then
  log_error "Something failed"
  return 1
fi
```

**Why avoid wrapper functions:**
- Makes error handling explicit and clear
- No magic behavior hiding what happens
- Easier to trace and understand code flow
- Forces explicit decision at each error point

### Exit vs Return

**NEVER** use `exit` in functions. Always use `return` instead, even in `main()`:

```bash
# ✗ WRONG
function some_function() {
  if [[ error ]]; then
    exit 1
  fi
}

# ✗ WRONG
main() {
  if [[ error ]]; then
    exit 1
  fi
}

# ✓ CORRECT
function some_function() {
  if [[ error ]]
  then
    return 1
  fi
}

# ✓ CORRECT
main() {
  if [[ error ]]
  then
    return 1
  fi
}
```

**Exit codes:**
- `0` - Success
- `1` - General errors
- `2` - Invalid command-line arguments/flags

### Code Style

#### Conditionals and Loops

Always put `then`, `do`, `else` on a new line (no semicolons):

```bash
# ✗ WRONG
if [[ condition ]]; then
  action
fi

# ✓ CORRECT
if [[ condition ]]
then
  action
fi

# ✗ WRONG
while read line; do
  process
done

# ✓ CORRECT
while read line
do
  process
done

# ✗ WRONG
for item in list; do
  process
done

# ✓ CORRECT
for item in list
do
  process
done
```

#### Case Statements

Always use multi-line format for case statements (no actions on the same line as pattern):

```bash
# ✗ WRONG - action on same line
case "$var" in
  opt1) action ;;
  opt2) action ;;
  *) default ;;
esac

# ✗ WRONG - semicolons on same line
case "$var" in
  opt1) action1; action2 ;;
esac

# ✓ CORRECT - multi-line format
case "$var" in
  opt1)
    action
    ;;
  opt2)
    action1
    action2
    ;;
  *)
    default_action
    ;;
esac
```

#### Variable Declarations

Declare and assign variables separately to avoid masking return values:

```bash
# ✗ WRONG (shellcheck SC2155)
local result=$(command)

# ✓ CORRECT
local result
if ! result=$(command 2>/dev/null)
then
  echo "ERROR: command failed"
  return 1
fi
```

### YAML Parsing

Use `yq` (Go version from https://github.com/mikefarah/yq) for YAML parsing:

```bash
# ✓ CORRECT
if ! hostname=$(yq '.remote.hostname' "$config_file" 2>/dev/null)
then
  die "Failed to parse hostname from config"
fi
```

### Path Expansion

**Always support** tilde (`~`) and `$HOME` expansion in paths:

```bash
# Configuration examples that must work:
local:
  root: ~/Videos           # Expands to /home/user/Videos
  directories:
    docs: $HOME/Documents  # Expands to /home/user/Documents
    abs: /absolute/path    # Remains unchanged

# Implementation:
expand_home() {
  local path_str="$1"

  # Expand $HOME
  path_str="${path_str//\$HOME/$HOME}"

  # Expand ~ at the beginning
  if [[ "$path_str" =~ ^~ ]]
  then
    path_str="${path_str/#\~/$HOME}"
  fi

  echo "$path_str"
}

# Use in all path handling functions
base_root=$(expand_home "$base_root")
```

### Boolean Variables

**DO NOT** use `"true"` or `"false"` as string values for booleans. Use `1` for true, empty for false:

```bash
# ✗ WRONG - string booleans
DRY_RUN=false
VERBOSE=true
if [[ "$VERBOSE" == "true" ]]
then
  echo "verbose"
fi

# ✓ CORRECT - numeric/empty booleans
DRY_RUN=
VERBOSE=1
if [[ -n "$VERBOSE" ]]
then
  echo "verbose"
fi

# ✗ WRONG - checking false
if [[ "$var" == "false" ]]
then
  action
fi

# ✓ CORRECT - checking empty
if [[ -z "$var" ]]
then
  action
fi
```

**Summary:**
- Set variable: `VAR=1` (true) or `VAR=` (false)
- Check true: `if [[ -n "$VAR" ]]`
- Check false: `if [[ -z "$VAR" ]]`

### Variable Naming

**AVOID** using variable names that conflict with common environment variables or shell built-ins:

```bash
# ✗ WRONG - conflicts with PATH environment variable
local path="/some/path"
for path in "${paths[@]}"

# ✓ CORRECT - use descriptive, non-conflicting names
local file_path="/some/path"
local target_path="/some/path"
for filepath in "${paths[@]}"

# Other variables to avoid:
# - PATH, HOME, USER, SHELL, PWD
# - IFS (unless intentionally modifying)
# - LANG, LC_* (locale variables)
```

**Always use lowercase** for local variable names:

```bash
# ✗ WRONG - uppercase local variables
local RESULT="value"
local PAIRS

# ✓ CORRECT - lowercase local variables
local result="value"
local pairs
```

**NEVER declare local inside loops** - Declare ONCE before the loop, (re)initialize inside:

```bash
# ✗ WRONG - local declared in loop
for item in "${items[@]}"
do
  local tmp="tmp-$item"
  local result
  result=$(process "$tmp")
done

# ✗ WRONG - even without assignment
while IFS='|' read -ra pairs
do
  local pair  # ✗ Still wrong!
  for pair in "${pairs[@]}"
  do
    local key  # ✗ Wrong!
    local value  # ✗ Wrong!
    process "$key" "$value"
  done
done

# ✓ CORRECT - declare ONCE before loop, assign inside
local tmp
local result
for item in "${items[@]}"
do
  tmp="tmp-$item"
  result=$(process "$tmp")
done

# ✓ CORRECT - all locals before loops
local pair
local key
local value
while IFS='|' read -ra pairs
do
  for pair in "${pairs[@]}"
  do
    IFS='=' read -r key value <<< "$pair"
    process "$key" "$value"
  done
done
```

**Rule:** The word `local` should NEVER appear inside a loop body (for/while/until).

### Code Cleanliness

**NO trailing whitespace** or lines with only whitespace:

```bash
# ✗ WRONG - trailing spaces
function test() {␣␣
  echo "hello"␣
}

# ✓ CORRECT - no trailing whitespace
function test() {
  echo "hello"
}
```

Check for trailing whitespace:
```bash
grep -n '[[:space:]]$' script.sh
```

### Shellcheck Compliance

All scripts **must** pass `shellcheck` without warnings or errors:

```bash
shellcheck jellysync
```

Common fixes:
- SC2155: Declare and assign separately
- SC2086: Quote variables
- SC2046: Quote command substitutions
- SC2068: Quote array expansions with `"${array[@]}"`

### Function Arguments

Use clear, self-documenting function interfaces:

```bash
# ✗ WRONG - unclear what 1 or "" mean
result=$(expand_path "$dir" 1 "$name")
result=$(expand_path "$dir" "" "$name")

# ✓ CORRECT - use flags for clarity
result=$(expand_path --remote "$dir" "$name")
result=$(expand_path --local "$dir" "$name")

# ✗ WRONG - positional boolean arguments
process_file "$filename" 1 "" 1

# ✓ CORRECT - named flags
process_file --verbose --no-backup --force "$filename"
```

**Prefer flags over positional arguments when:**
- The argument is a boolean or mode selector
- The meaning isn't immediately obvious
- Multiple optional parameters exist

### Global State Variables

Prefer global variables over parameter passing for application-wide state:

```bash
# ✗ WRONG - passing state through parameters
sync_jobs() {
  local dry_run="$1"
  shift
  local target_jobs=("$@")
  # ...
  sync_job "$job_data" "$dry_run"
}

# ✓ CORRECT - using global state
DRY_RUN=
sync_jobs() {
  local target_jobs=("$@")
  # ...
  sync_job "$job_data"  # Uses global DRY_RUN
}
```

**When to use globals:**
- Application-wide flags (DRY_RUN, VERBOSE, DEBUG)
- Configuration data (loaded once, used everywhere)
- Avoids parameter drilling through multiple function layers

**When to use parameters:**
- Data specific to function operation
- Values that change per invocation
- Makes function dependencies explicit

### Function Structure

```bash
function_name() {
  local param="$1"
  local result

  # Validate input
  if [[ -z "$param" ]]
  then
    log_error "Missing required parameter"
    return 1
  fi

  # Perform operation with error checking
  if ! result=$(some_command "$param" 2>/dev/null)
  then
    log_error "Command failed"
    return 1
  fi

  echo "$result"
}
```

### Main Function Pattern

Always wrap script execution in a main function with proper guard:

```bash
main() {
  # Parse arguments
  while [[ $# -gt 0 ]]
  do
    case "$1" in
      -h|--help)
        usage
        return 0
        ;;
      -*)
        log_error "Unknown option: $1"
        return 2  # Exit code 2 for invalid flags
        ;;
      *)
        # Handle positional arguments
        shift
        ;;
    esac
  done

  # Script logic here
  if ! some_command
  then
    return 1
  fi

  return 0
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]
then
  main "$@"
fi
```

This allows the script to be sourced for testing without executing.

**Important:** Use `return` everywhere, never `exit` in functions (including `main()`).

## Dependencies

- `bash` 4.0+
- `yq` (Go version: mikefarah/yq)
- `rsync`
- `ssh`

Check dependencies explicitly:

```bash
check_dependencies() {
  local missing=()

  if ! command -v yq &>/dev/null
  then
    missing+=(yq)
  fi

  if [[ ${#missing[@]} -gt 0 ]]
  then
    die "Missing required commands: ${missing[*]}"
  fi
}
```

## Configuration

- Use YAML for configuration files
- Parse with `yq` (Go version)
- Validate all required fields explicitly
- Provide helpful error messages

## Testing

Before committing:

1. Run `shellcheck` on all bash scripts
2. Test with `--dry-run` flag
3. Test `--list` command
4. Test individual job sync
5. Test error handling (missing config, invalid paths, etc.)

## License

This project is licensed under the **GNU General Public License v3.0 (GPLv3)**.

All source files must include the GPL header:
```bash
#!/usr/bin/env bash
#
# [script name] - [description]
# Copyright (C) [year] [author]
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
```

## Code Review Checklist

- [ ] No `set -e` or `|| true` patterns
- [ ] All `then`/`do`/`else` on new lines (no semicolons)
- [ ] Case statements use multi-line format
- [ ] Variables declared and assigned separately
- [ ] Error checking on all external commands
- [ ] No `exit` calls in functions (use `return` instead)
- [ ] Invalid flags return exit code 2
- [ ] Booleans use `1` for true, empty for false (not "true"/"false" strings)
- [ ] Boolean checks use `[[ -n ]]` and `[[ -z ]]`
- [ ] Variable names don't conflict with environment variables (PATH, HOME, etc.)
- [ ] Local variables are lowercase
- [ ] No `local` declarations inside loops
- [ ] Function arguments use clear flags instead of magic values
- [ ] Path expansion supports `~` and `$HOME`
- [ ] No trailing whitespace
- [ ] `shellcheck` passes without warnings
- [ ] Functions have proper error handling
- [ ] User-facing errors have helpful messages
- [ ] GPL license header present in all source files

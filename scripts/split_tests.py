#!/usr/bin/env python3
# Copied from Manifold-Game/manifold, tools/dev/split_tests.py at def2bab39, where its tests live.
# Change it there and copy it here; do not edit this copy. In this repository it is run as
# `python3 scripts/split_tests.py <paths>`, and CI runs `python3 scripts/split_tests.py --check crates`.
"""Move unit test modules out of source files into the crate's tests/src folder, mirroring src.

Use this when a source file holds a `#[cfg(test)]` module: unit tests live under tests/src, not in src.

    ./dev split-tests crates/manifold-world            # every source file of a crate
    ./dev split-tests crates/manifold-world/src/a.rs   # one file
    ./dev split-tests --check crates                   # only report what would move

The standard (AGENTS.md, "Unit tests live under tests/src"): a file under `src/` holds product code
only. The unit tests of `src/<path>/<file>.rs` are in `tests/src/<path>/<file>_test.rs` of the same
crate, and the source file points at them:

    #[cfg(test)]
    #[path = "../../tests/src/<path>/<file>_test.rs"]
    mod tests;

The tests stay a child module of the source file, so `use super::*;` and private items work as
before, and no test is renamed. A test module inside an inline module (`mod store { .. }`) points
the same way, and the inline module gets `#[path = "."]`: without it the pointer would resolve
from the folder `src/<file>/store`, which is not on disk, and Linux cannot walk through it.
For each source file this tool:

1. moves every `#[cfg(test)] mod name { .. }` body to its file under `tests/src` and leaves the
   three-line pointer (a second test module `name` of `a.rs` goes to `a_name_test.rs`);
2. moves a test module that is already its own file under `src/` the same way: one declared
   `#[cfg(test)] mod tests;`, or a plain `mod tests;` whose file opens with `#![cfg(test)]`
   (the declaration then gets the `#[cfg(test)]`);
3. corrects a pointer that no longer names the mirrored place, moving the test file there (after a
   source file was moved or renamed);
4. rewrites relative `include_str!`, `include_bytes!` and `include!` paths in the moved tests.

It leaves alone, and lists with the reason: a test module inside a function body, one that already
has a `#[path]` or declares child modules in other files, and one whose target file exists. Run
`cargo fmt` afterwards: the moved code loses one level of indentation. Folders named `target`,
`node_modules` and `.git`, and `tools/patches`, are not walked.

Changes files: writes under `tests/src`, edits the source files. Never interactive. Exit status:
0 done (or, with --check, nothing to move), 1 something was left alone (or, with --check, something
would move), 2 usage error.
"""
import argparse
import json
import os
from pathlib import Path
import re
import sys

PURPOSE = "Move unit test modules out of source files into the crate's tests/src folder, mirroring src."

CFG_ATTR = re.compile(r'#\[cfg\((.*)\)\][ \t]*\r?$', re.M)
INNER_TEST_CFG = re.compile(r'\A(?:[ \t]*//[^\n]*\n|[ \t]*\r?\n)*[ \t]*#!\[cfg\(test\)\]')
MOD_HEAD = re.compile(r'(?:^|\s)(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?([A-Za-z_][A-Za-z_0-9]*)\s*$')
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]*)"\s*\]')
# the path literal of an include, or the literal that opens its `concat!(..)`
INCLUDE = re.compile(r'\b(include_str|include_bytes|include)!\(\s*(?:concat!\(\s*)?"([^"]+)"')
CHILD_FILE_MOD = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z_0-9]*\s*;', re.M)
STRING_START = re.compile(r'(?:b|c)?(r?)(#*)"')
SKIP_DIRS = {'target', 'node_modules', '.git'}
ROOT_NAMES = ('mod.rs', 'lib.rs', 'main.rs')


def lex(text):
    """The braces and semicolons of `text` that are code, in order, and the spans of its string
    literals. Comments, strings, character literals and lifetimes are stepped over."""
    i, n = 0, len(text)
    events, strings = [], []
    while i < n:
        c = text[i]
        if c == '/' and text.startswith('//', i):
            j = text.find('\n', i)
            i = n if j < 0 else j
            continue
        if c == '/' and text.startswith('/*', i):
            depth, i = 1, i + 2
            while i < n and depth:
                if text.startswith('/*', i):
                    depth, i = depth + 1, i + 2
                elif text.startswith('*/', i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            continue
        starts_word = i == 0 or not (text[i - 1].isalnum() or text[i - 1] == '_')
        m = STRING_START.match(text, i) if (c == '"' or (c in 'rbc' and starts_word)) else None
        if m:
            start, i = i, m.end()
            if m.group(1):                       # raw: ends at `"` and the same hashes
                end = text.find('"' + m.group(2), i)
                i = n if end < 0 else end + 1 + len(m.group(2))
            else:
                while i < n and text[i] != '"':
                    i += 2 if text[i] == '\\' else 1
                i += 1
            strings.append((start, i))
            continue
        if c == "'":
            if text.startswith('\\', i + 1):     # '\n', '\'', '\u{..}'
                j = text.find("'", i + 3)
                i = n if j < 0 else j + 1
            elif i + 2 < n and text[i + 2] == "'":
                i += 3
            else:
                i += 1                           # a lifetime or a label
            continue
        if c in '{};':
            events.append((c, i))
        i += 1
    return events, strings


def read_text(path):
    """A file's text with its line endings as they are. (`Path.read_text` takes `newline` only
    from Python 3.13; CI runs 3.10.)"""
    with open(path, encoding='utf-8', newline='') as file:
        return file.read()


def split_arguments(text):
    """`text` split at its top-level commas (not those inside parentheses or strings)."""
    parts, depth, start, quoted = [], 0, 0, False
    for i, c in enumerate(text):
        if c == '"':
            quoted = not quoted
        elif quoted:
            continue
        elif c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
        elif c == ',' and depth == 0:
            parts.append(text[start:i])
            start = i + 1
    parts.append(text[start:])
    return [p.strip() for p in parts if p.strip()]


def requires_test(predicate):
    """Whether a `cfg` predicate holds only under test: `test`, or `all(..)` with such a part.
    (`any(test, ..)` also builds outside tests, and `not(test)` is product code.)"""
    predicate = predicate.strip()
    if predicate == 'test':
        return True
    m = re.match(r'all\s*\((.*)\)$', predicate, re.S)
    return bool(m) and any(requires_test(part) for part in split_arguments(m.group(1)))


def is_test_only(attributes):
    """Whether the attribute lines `attributes` carry a test-only `#[cfg(..)]`."""
    return any(requires_test(m.group(1)) for m in CFG_ATTR.finditer(attributes))


def without_comment_lines(segment):
    return '\n'.join(line for line in segment.split('\n') if not line.lstrip().startswith('//'))


def test_modules(text):
    """The test-only modules `text` declares, outermost only. Each is a dict: `name`, `enclosing`
    (the inline modules around it), `head` (where its attributes start), `mod_at` (the `mod`
    keyword's line start), `path` (its `#[path]` value and span, or None), and either `open`/`close`
    (an inline module's braces) or `semi` (a `mod name;`). `parents` describes the enclosing inline
    modules the same way (their `mod_at` and `path`). `in_block` is true inside a function body
    or other non-module braces. A `mod name;` with no test-only `cfg` is returned too, marked
    `plain`: its file may say `#![cfg(test)]` itself."""
    events, strings = lex(text)
    stack = []            # (is a module, name, is test-only)
    boundary = 0
    found = []

    def describe(pos, name):
        segment = text[boundary:pos]
        mod_kw = boundary + max(m.start() for m in re.finditer(r'\bmod\b', segment))
        path = None
        for m in PATH_ATTR.finditer(segment):
            if not text[text.rfind('\n', 0, boundary + m.start()) + 1:boundary + m.start()].lstrip().startswith('//'):
                path = (m.group(1), boundary + m.start(1), boundary + m.end(1))
        return {'name': name, 'enclosing': [e[1] for e in stack], 'path': path,
                'parents': [e[4] for e in stack if e[4] is not None],
                'mod_at': text.rfind('\n', 0, mod_kw) + 1,
                'in_block': not all(e[0] for e in stack)}

    for kind, pos in events:
        if kind == '}':
            if stack:
                closed = stack.pop()
                if closed[3] is not None:
                    closed[3]['close'] = pos
            boundary = pos + 1
            continue
        segment = without_comment_lines(text[boundary:pos])
        m = MOD_HEAD.search(segment)
        is_test = bool(m and is_test_only(segment))
        record = None
        if is_test and not any(e[2] for e in stack):
            record = describe(pos, m.group(1))
            record['open' if kind == '{' else 'semi'] = pos
            found.append(record)
        elif m and kind == ';' and not any(e[2] for e in stack) and all(e[0] for e in stack):
            plain = describe(pos, m.group(1))
            plain.update(semi=pos, plain=True)
            found.append(plain)
        if kind == '{':
            parent = describe(pos, m.group(1)) if m else None
            stack.append((bool(m), m.group(1) if m else None, is_test or any(e[2] for e in stack), record, parent))
        boundary = pos + 1
    return [f for f in found if 'semi' in f or 'close' in f], strings


def crate_root(path):
    """The package folder of a source file: the nearest ancestor with a Cargo.toml, when the file
    is under its `src`."""
    for folder in path.parents:
        if (folder / 'Cargo.toml').is_file():
            try:
                path.relative_to(folder / 'src')
            except ValueError:
                return None
            return folder
    return None


def is_crate_root(path, crate):
    """Whether `path` is the root file of a target (its modules resolve from its own folder)."""
    if path.name in ('lib.rs', 'main.rs'):
        return True
    if path.parent.name == 'bin' and path.parent.parent == crate / 'src':
        return True
    try:
        manifest = (crate / 'Cargo.toml').read_text(encoding='utf-8')
    except OSError:
        return False
    declared = {(crate / m.group(1)).resolve() for m in re.finditer(r'^\s*path\s*=\s*"([^"]+\.rs)"', manifest, re.M)}
    return path.resolve() in declared


def children_dir(path, crate):
    """The folder a file's `mod x;` children are found in."""
    return path.parent if path.name == 'mod.rs' or is_crate_root(path, crate) else path.with_suffix('')


def inline_dir(path, crate, parents):
    """The folder the children of the inline modules `parents` (outermost first) are found in: an
    inline `mod a { .. }` moves into `a/`, or into its `#[path]`, which is relative to the folder
    of the module around it (for the file's own level, the file's folder, without its stem)."""
    here, around = children_dir(path, crate), path.parent
    for parent in parents:
        here = around / parent['path'][0] if parent['path'] else here / parent['name']
        around = here
    return here


def pointer_base(path, crate, parents):
    """The folder a `#[path]` on a module declared in `path`, inside the inline modules `parents`,
    is relative to."""
    return inline_dir(path, crate, parents) if parents else path.parent


def mirror_target(path, crate, enclosing, name):
    """`<crate>/tests/src/<the file's path under src>_test.rs`; a module other than `tests` adds its
    name (without a trailing `_tests`)."""
    inner = path.relative_to(crate / 'src').with_suffix('')
    parts = [inner.name] + list(enclosing)
    if name not in ('tests', 'test'):
        parts.append(re.sub(r'_tests?$', '', name))
    return crate / 'tests' / 'src' / inner.parent / ('_'.join(parts) + '_test.rs')


def relative(target, base):
    return os.path.relpath(target, base).replace('\\', '/')


def dedent(body, open_pos, indent, strings):
    """`body` (the text after the brace at `open_pos`) with `indent` leading spaces removed from
    each line that does not begin inside a string literal."""
    out, offset = [], open_pos + 1
    pad = ' ' * indent
    for line in body.split('\n'):
        inside = any(a < offset < b for a, b in strings)
        out.append(line[indent:] if (not inside and line.startswith(pad)) else line)
        offset += len(line) + 1
    return '\n'.join(out)


def rewrite_includes(body, old_dir, new_dir):
    def fix(m):
        written = m.group(2)
        if written.startswith('/') or re.match(r'^[A-Za-z]:', written):
            return m.group(0)
        moved = relative(old_dir / written, new_dir) + ('/' if written.endswith('/') else '')
        return m.group(0).replace(f'"{written}"', f'"{moved}"')
    return INCLUDE.sub(fix, body) if old_dir != new_dir else body


def plan_file(path, check_only):
    """Move what `path` holds; returns (moved, skipped) as lists of dicts."""
    crate = crate_root(path)
    moved, skipped = [], []
    if crate is None:
        return moved, skipped
    try:
        text = read_text(path)
    except (OSError, UnicodeDecodeError):
        return moved, skipped
    if 'mod ' not in text:
        return moved, skipped
    modules, strings = test_modules(text)
    newline = '\r\n' if '\r\n' in text else '\n'
    edits = []            # (start, end, replacement), applied from the end of the file
    writes = []           # (target, content) and file moves, done once every edit is known
    renames = []
    claimed = set()
    anchored = set()      # inline modules already given `#[path = "."]`

    def skip(module, reason):
        skipped.append({'source': str(path), 'module': module['name'], 'reason': reason})

    for module in modules:
        name, enclosing = module['name'], module['enclosing']
        plain = module.get('plain', False)
        if plain:
            # a module declared without a test-only `cfg`: a test module only when its own file,
            # under `src/`, opens with `#![cfg(test)]`
            home = inline_dir(path, crate, module['parents'])
            declared = (pointer_base(path, crate, module['parents']) / module['path'][0]) if module['path'] else home / f'{name}.rs'
            folder = home / name / 'mod.rs'
            try:
                if declared.is_file():
                    declared.resolve().relative_to((crate / 'src').resolve())
                    if not INNER_TEST_CFG.match(read_text(declared)):
                        continue
                elif not module['path'] and folder.is_file() and INNER_TEST_CFG.match(read_text(folder)):
                    skip(module, 'a test module that is a folder (`mod.rs`): move it by hand')
                    continue
                else:
                    continue
            except (OSError, UnicodeDecodeError, ValueError):
                continue
        if module['in_block']:
            skip(module, 'inside a function body or other block: `mod x;` is not allowed there')
            continue
        target = mirror_target(path, crate, enclosing, name)
        # Inside an inline module the pointer would resolve from a folder that is not on disk
        # (`src/<file>/<module>`), which only Windows tolerates. Each enclosing inline module gets
        # `#[path = "."]`: its children then resolve from this file's own folder, as at file level.
        parents = module['parents']
        if any(p['path'] and p['path'][0] != '.' for p in parents):
            skip(module, 'inside an inline module that has its own `#[path]`: move it by hand')
            continue
        if parents and any(m is not module and m.get('plain') and m['enclosing'] for m in modules):
            skip(module, 'its inline module declares other modules in files: move it by hand')
            continue
        anchors = []
        for parent in parents:
            if not parent['path'] and parent['mod_at'] not in anchored:
                rest = text[parent['mod_at']:]
                anchors.append((parent['mod_at'], parent['mod_at'],
                                f'{rest[:len(rest) - len(rest.lstrip(" "))]}#[path = "."]{newline}'))
        base = pointer_base(path, crate, parents)       # where the declaration resolves from today
        pointer = relative(target, path.parent)

        def anchor():
            for edit in anchors:
                if edit[0] not in anchored:
                    anchored.add(edit[0])
                    edits.append(edit)
        indent_text = text[module['mod_at']:]
        pad = indent_text[:len(indent_text) - len(indent_text.lstrip(' '))]
        if 'open' in module:
            if module['path']:
                skip(module, 'an inline module with a `#[path]`: move it by hand')
                continue
            body = text[module['open'] + 1:module['close']]
            if CHILD_FILE_MOD.search(body) or PATH_ATTR.search(body):
                skip(module, 'declares child modules in other files: move it by hand')
                continue
            if target.exists() or target in claimed:
                skip(module, f'{target} already exists')
                continue
            content = dedent(body, module['open'], len(pad) + 4, strings).lstrip('\r\n').rstrip() + newline
            writes.append((target, rewrite_includes(content, path.parent, target.parent)))
            edits.append((module['mod_at'], module['mod_at'], f'{pad}#[path = "{pointer}"]{newline}'))
            edits.append((len(text[:module['open']].rstrip()), module['close'] + 1, ';'))
            anchor()
        else:
            if module['path']:
                current = (base / module['path'][0])
            else:
                home = inline_dir(path, crate, parents)
                current = home / f'{name}.rs'
                if not current.is_file() and (home / name / 'mod.rs').is_file():
                    skip(module, 'a test module that is a folder (`mod.rs`): move it by hand')
                    continue
            if current.is_file() and current.resolve() == target.resolve() and not anchors:
                continue                                    # already where it belongs
            if current.is_file() and current.resolve() == target.resolve():
                # in place, but pointed at through an inline module's folder: anchor it
                edits.extend(pointer_edits(module, pad, pointer, newline))
                anchor()
                moved.append({'source': str(path), 'module': name, 'test_file': str(target), 'kind': 'pointer'})
                continue
            if not current.is_file():
                # the source file moved: look for the test file by its name under tests/src
                named = [p for p in (crate / 'tests' / 'src').rglob(target.name)] if (crate / 'tests' / 'src').is_dir() else []
                if len(named) != 1:
                    skip(module, f'its file {current} does not exist')
                    continue
                current = named[0]
                if current.resolve() == target.resolve():
                    edits.extend(pointer_edits(module, pad, pointer, newline))
                    anchor()
                    moved.append({'source': str(path), 'module': name, 'test_file': str(target), 'kind': 'pointer'})
                    continue
            try:
                child_text = read_text(current)
            except (OSError, UnicodeDecodeError):
                skip(module, f'cannot read {current}')
                continue
            if CHILD_FILE_MOD.search(without_comment_lines(child_text)) or PATH_ATTR.search(child_text):
                skip(module, 'declares child modules in other files: move it by hand')
                continue
            if target.exists() or target in claimed:
                skip(module, f'{target} already exists')
                continue
            renames.append((current, target, rewrite_includes(child_text, current.parent, target.parent)))
            edits.extend(pointer_edits(module, pad, pointer, newline))
            anchor()
        claimed.add(target)
        moved.append({'source': str(path), 'module': name, 'test_file': str(target),
                      'kind': 'inline' if 'open' in module else 'file'})
    if check_only or not edits:
        return moved, skipped
    for target, content in writes:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding='utf-8', newline='')
    for current, target, content in renames:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding='utf-8', newline='')
        current.unlink()
        folder = current.parent
        while folder != crate and folder.is_dir() and not any(folder.iterdir()):
            folder.rmdir()
            folder = folder.parent
    for start, end, replacement in sorted(edits, reverse=True):
        text = text[:start] + replacement + text[end:]
    path.write_text(text, encoding='utf-8', newline='')
    return moved, skipped


def pointer_edits(module, pad, pointer, newline):
    """The edits that make a `mod name;` point at `pointer`: replace its `#[path]` value, or add
    one; a module that was test-only through its file's `#![cfg(test)]` gets `#[cfg(test)]` too."""
    cfg = f'{pad}#[cfg(test)]{newline}' if module.get('plain') else ''
    if module['path']:
        edits = [(module['path'][1], module['path'][2], pointer)]
        return edits + ([(module['mod_at'], module['mod_at'], cfg)] if cfg else [])
    return [(module['mod_at'], module['mod_at'], f'{cfg}{pad}#[path = "{pointer}"]{newline}')]


def sources(paths):
    """The source files under `paths`: `.rs` files under a `src` folder that are not themselves
    under a `tests` folder."""
    seen = set()
    for given in paths:
        given = Path(given).resolve()
        if given.is_file():
            files = [given]
        else:
            files = []
            for folder, names, leaves in os.walk(given):
                names[:] = sorted(n for n in names if n not in SKIP_DIRS
                                  and not (n == 'patches' and Path(folder).name == 'tools'))
                files.extend(Path(folder) / leaf for leaf in sorted(leaves) if leaf.endswith('.rs'))
        for file in files:
            parts = file.parts
            if file in seen or 'src' not in parts:
                continue
            at = len(parts) - 1 - parts[::-1].index('src')      # the innermost `src`
            if 'tests' in parts[at + 1:-1] or (at > 0 and parts[at - 1] == 'tests'):
                continue                                         # a test folder: not a source file
            seen.add(file)
            yield file


class Parser(argparse.ArgumentParser):
    def format_help(self):
        return PURPOSE + '\n\n' + super().format_help()


def main(argv=None):
    parser = Parser(prog='./dev split-tests',
                    description='Unit tests live under tests/src, mirroring src; see the module docs for the rules.')
    parser.add_argument('paths', nargs='*', help='source files, or folders to walk')
    parser.add_argument('--check', action='store_true', help='report what would move; write nothing')
    parser.add_argument('--json', action='store_true', help='machine output: one JSON object on stdout')
    args = parser.parse_args(argv)
    if not args.paths:
        print('split-tests: name the files or folders to convert (for example `crates/manifold-world`)', file=sys.stderr)
        return 2
    missing = [p for p in args.paths if not Path(p).exists()]
    if missing:
        print(f'split-tests: no such path: {missing[0]}', file=sys.stderr)
        return 2
    moved, skipped = [], []
    # A test module that is its own file is found from its parent; do not read it as a source.
    files = list(sources(args.paths))
    for file in files:
        if not file.exists():
            continue                      # a test file an earlier source just moved
        done, left = plan_file(file, args.check)
        moved.extend(done)
        skipped.extend(left)
    here = Path.cwd()

    def short(p):
        try:
            return Path(p).relative_to(here).as_posix()
        except ValueError:
            return Path(p).as_posix()

    if args.json:
        for row in moved + skipped:
            row['source'] = short(row['source'])
            if 'test_file' in row:
                row['test_file'] = short(row['test_file'])
        print(json.dumps({'checked_only': args.check, 'files': len(files), 'moved': moved, 'skipped': skipped}, indent=1))
    else:
        verb = 'would move' if args.check else 'moved'
        print(f'split-tests: {len(files)} source files read; {verb} {len(moved)} test modules '
              f'out of {len({m["source"] for m in moved})} files; left {len(skipped)} alone')
        for row in moved[:8]:
            print(f'  {short(row["source"])} `{row["module"]}` -> {short(row["test_file"])}')
        if len(moved) > 8:
            print(f'  ... {len(moved) - 8} more; --json lists all')
        for row in skipped[:20]:
            print(f'  left: {short(row["source"])} `{row["module"]}`: {row["reason"]}')
        if len(skipped) > 20:
            print(f'  ... {len(skipped) - 20} more; --json lists all')
        if moved and not args.check:
            print('  run `cargo fmt` (and scripts/fmt-standalone.sh): the moved tests lost one level of indentation')
    if args.check:
        return 1 if (moved or skipped) else 0
    return 1 if skipped else 0


if __name__ == '__main__':
    sys.exit(main())

#!/usr/bin/env python3
"""One tolerant reader for a Cargo manifest's `name = "..."` line (issue #494).

Four scripts in this directory classified the same identifier and drifted
apart. `check_unlinted_crates.py` learned to tolerate a trailing `# comment`
(#464) and then a quoted key (#486); the other three learned neither, and
nothing reported that, because every real `[package] name` in this tree is
the plain unquoted uncommented spelling all four already read. The next crate
added with a single-quoted value or a quoted key would be dropped from one
guard's corpus and read *wrong* by another -- `check_test_filter_coverage.py`
falls back to the directory's last path component when its regex misses, and
that component is not the package name for every member.

So the pattern lives here once. `tests/test_package_name_extractors.py` is
the ratchet: it compiles and runs every string constant in `scripts/*.py` and
demands the full tolerance set from anything that behaves like a reader of
this line, so a fifth copy cannot be added un-fixed.

No TOML parser: `ci.yml` invokes `python3` with no `actions/setup-python`
step, so nothing in this repo pins the runner's Python to >= 3.11 and
`tomllib` cannot be assumed. Each caller's own module docstring gives the
rest of that reasoning.
"""

from __future__ import annotations

import re

# A whole `name = "..."` line, in every spelling TOML allows for it:
#
#   name = "foo"                  the spelling every manifest here uses today
#   name = "foo" # explanation    trailing comment (#464)
#   "name" = "foo"                quoted key (#486)
#   name = 'foo'                  single-quoted value (#494)
#
# `re.MULTILINE` so `^`/`$` anchor per line, which lets the same pattern be
# `search`ed over a multi-line block (a `[[bin]]` entry) and `match`ed against
# one already-stripped line. The leading `\s*` is what makes the block use
# safe; `package_name()` below strips first and does not depend on it.
#
# The end anchor is deliberate: without it `name = "foo" extra` would read as
# `foo`, and a line this pattern does not understand must read as "no name"
# so the caller reports a gap someone looks at, never a confident wrong name.
NAME_LINE = re.compile(
    r"""^\s*["']?name["']?\s*=\s*["']([^"']+)["']\s*(?:#.*)?$""", re.MULTILINE
)


def name_in_block(text: str) -> str | None:
    """The first `name = "..."` in `text`, or None if there is none.

    For callers that have already narrowed `text` to one table's body -- a
    `[[bin]]` entry, say. It does no section tracking of its own, so handing
    it a whole manifest returns whichever `name` comes first in the file,
    which is why `package_name()` exists separately.
    """
    m = NAME_LINE.search(text)
    return m.group(1) if m else None


def package_name(toml_text: str) -> str | None:
    """The `[package]` table's `name`, or None for a virtual manifest.

    Scoped to the `[package]` table rather than searching the whole file: a
    manifest may declare `[[bin]] name = "..."` (this workspace has several,
    and `xai-grok-pager-bin`'s bin target is deliberately named something
    else), and a whole-file search returns whichever comes first in the file.
    Any other table header ends the search: reaching one means `[package]`
    had no `name`, which is not something to keep looking for elsewhere.
    """
    in_package = False
    for line in toml_text.splitlines():
        stripped = line.strip()
        if stripped == "[package]":
            in_package = True
            continue
        if stripped.startswith("[") and stripped.endswith("]"):
            if in_package:
                return None
            continue
        if in_package:
            m = NAME_LINE.match(stripped)
            if m:
                return m.group(1)
    return None

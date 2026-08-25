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

No TOML parser here, and that is a **choice rather than a constraint** --
stated carefully, because the earlier wording of this paragraph claimed more
than the facts support.

What is true: `ci.yml` invokes `python3` with no `actions/setup-python` step,
so nothing in this repo **pins** the runner's Python. What is also true, and
measured: the runner image is `ubuntu-24.04`, whose Python is 3.12, so
`tomllib` is in fact available today. Unpinned is not the same as absent, and
this paragraph used to conflate them.

So the reason production does not use `tomllib` is not that it cannot. It is
that these are *guards*, and a guard that answers one way where it is
authored and another way where it runs is the #171 failure exactly. Depending
on an unpinned interpreter feature would reintroduce that axis for no gain:
this reader is measured to agree with `tomllib` on 83 of 83 manifests in the
tree, so the parser would buy correctness this workspace never asks for.

The **test** may depend on `tomllib`, and does -- see
`DifferentialAgainstTomllib`, which imports it unconditionally so that the
day the assumption stops holding is a red build rather than a silent skip.
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

# A table header line. Group `key` is the raw key between the brackets, with
# insignificant whitespace already outside the capture; `open` says whether
# this is an array-of-tables (`[[bin]]`).
#
# Everything TOML lets you vary here has to be varied here, because the
# comparison this feeds decides which table a `name` belongs to, and a header
# that is not recognised is wrong in two directions at once:
#
#   [package] # metadata   the table is never ENTERED -- name goes missing,
#                          and `workspace_members()` then answers with the
#                          directory basename, a wrong package name
#   [features] # x         the table is never LEFT -- a `name` under a later
#                          table is returned AS the package name
#
# The second is the one that hurts: a confident wrong answer, not a gap.
#
# Both of these are the same class of miss as the value side's, so both sides
# tolerate the same things: a trailing comment, a quoted key, and -- unlike
# the value side -- whitespace padding, which is insignificant in a header
# (`[ package ]` is the `package` table) but NOT inside quotes
# (`[" package "]` is a different table, whose key really does have spaces).
# That asymmetry is why the padding is stripped outside the quotes only.
#
# This is not a TOML validator and does not try to be: an unbalanced
# `[[package]` is not valid TOML, and whatever it is read as, it is not the
# `[package]` table.
_TABLE_HEADER = re.compile(
    r"^(?P<open>\[\[?)\s*(?P<key>[^\[\]]*?)\s*\]\]?\s*(?:#.*)?$"
)


def table_key(line: str) -> tuple[str, bool] | None:
    """`(key, is_array_of_tables)` for a table-header line, else None.

    The key is normalised the way TOML says to read it: padding outside any
    quotes is dropped, and one matching pair of surrounding quotes is
    removed. A dotted key (`package.metadata`) is returned whole, because it
    names a *different* table and the caller must not mistake it for its
    parent.

    Contract, stated because it is deliberately narrower than TOML: whatever
    is left after removing the quotes is compared byte for byte. Escape
    sequences in a basic (double-quoted) key are **not** decoded, so a header
    spelled with `\\u0061` in place of an `a` -- which TOML says still names
    the same table -- reads here as some other table. Decoding them is parser
    work, this is not a parser (see the module docstring), and the spelling
    appears in 0 of this workspace's 83 manifests. The one caller for which
    that miss is a change, `check_test_filter_coverage.workspace_members()`,
    is fail-closed on it: the fallback name reaches `cargo test -p`, the
    listing fails, and the run exits 2 saying it did not check that crate. A
    gap that announces itself is a limit worth accepting; a wrong answer
    would not be. `tests/test_package_name_extractors.py` pins the divergence
    against `tomllib` by name, so it stays a decision and not a surprise.

    Half of that is simply correct, incidentally: a literal (single-quoted)
    key never has escapes processed in TOML, so there is nothing to decode.
    """
    m = _TABLE_HEADER.match(line.strip())
    if m is None:
        return None
    key = m.group("key")
    if len(key) >= 2 and key[0] == key[-1] and key[0] in "\"'":
        key = key[1:-1]
    return key, m.group("open") == "[["


_MULTILINE_DELIMS = ('"""', "'" * 3)


def multiline_opener(stripped: str) -> str | None:
    """The multiline-string delimiter this line leaves open, or None.

    Checked in **value position only**, after splitting on the first `=`. A
    plain substring test would misfire on a single-line value that merely
    contains three escaped quotes in a row, which opens nothing.

    A string opened and closed on the same line opens nothing either, so that
    returns None too.

    Approximate on purpose, and the approximation is named: an *escaped*
    delimiter inside an already-open multiline string is read here as the
    closer. Telling those apart needs escape processing, which is the parser
    work this module does not do; it appears in 0 of this workspace's 83
    manifests, and the differential test names it.
    """
    _, sep, value = stripped.partition("=")
    if not sep:
        return None
    value = value.strip()
    for delim in _MULTILINE_DELIMS:
        if value.startswith(delim):
            return None if delim in value[len(delim) :] else delim
    return None


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

    Headers are read through `table_key()`, which normalises the spellings
    TOML allows -- see there for why an unrecognised header is wrong in two
    directions, and why the closing one is the worse of them.
    """
    in_package = False
    open_string: str | None = None
    for line in toml_text.splitlines():
        stripped = line.strip()

        # Inside a multiline string, nothing on the line is syntax. Skipping
        # only the header branch would not be enough: a `name = "..."` written
        # inside a `description` was returned AS the package name -- measured,
        # a manifest whose description contained one answered `sneaky` where
        # the real `[package] name` was `real`. A confident wrong answer is
        # the failure this whole guard family exists to prevent, so both
        # branches are skipped here.
        if open_string is not None:
            if open_string in stripped:
                open_string = None
            continue
        opener = multiline_opener(stripped)
        if opener is not None:
            open_string = opener
            continue

        header = table_key(stripped)
        if header is not None:
            key, is_array = header
            if key == "package" and not is_array:
                in_package = True
                continue
            if in_package:
                return None
            continue
        if in_package:
            m = NAME_LINE.match(stripped)
            if m:
                return m.group(1)
    return None

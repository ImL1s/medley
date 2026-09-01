#!/usr/bin/env python3
"""Reject negated GitHub closing keywords before a PR is merged (#513).

GitHub treats ``Does not close #123`` as a successful closing directive.  This
guard scans the PR title/body and every source commit message and requires
negative scope statements to use unambiguous wording such as
``Leaves #123 open`` instead.

Exit 0 for clean metadata, 1 for a finding, and 2 when the PR metadata cannot
be fetched or validated.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from collections import deque
from dataclasses import dataclass

NEGATION_WORD_WINDOW = 12  # compatibility alias; scanning is clause-scoped
MAX_REPORTED_FINDINGS = 20

_CLOSING = re.compile(
    r"\b(?P<keyword>close(?:s|d)?|fix(?:es|ed)?|resolve(?:s|d)?)\b"
    r"(?:\s*:\s*|\s+)"
    r"(?P<reference>#\d+|[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+#\d+"
    r"|https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/issues/\d+)\b",
    re.IGNORECASE,
)
_WORD = re.compile(
    r"(?<![\w])[A-Za-z][A-Za-z0-9_]*(?:['’][A-Za-z]+)?(?![\w])"
)
_CLAUSE_BOUNDARY = re.compile(r"[.?!;]")
_SCAN_TOKEN = re.compile(
    rf"(?P<boundary>{_CLAUSE_BOUNDARY.pattern})"
    r"|(?P<newline>\n)"
    r"|(?P<comma>,)"
    r"|(?P<dash>—|--)"
    r"|(?P<paren>[()])"
    rf"|(?P<word>{_WORD.pattern})"
)
_ADVERSATIVE = frozenset({"but"})
_SUBORDINATE_RESET = frozenset(
    {
        "because",
        "while",
        "when",
        "since",
        "although",
        "though",
        "whereas",
        # `unless` is conditional ("not complete unless it fixes") — keep
        # negation in scope rather than treating it as affirmative (#530).
    }
)
_NEGATION_WORDS = frozenset(
    {
        "not",
        "never",
        "cannot",
        "cant",
        "doesnt",
        "dont",
        "wont",
        "isnt",
        "arent",
        "wasnt",
        "werent",
        "hasnt",
        "havent",
        "hadnt",
        "couldnt",
        "shouldnt",
        "wouldnt",
        "didnt",
        "neither",
        "nor",
        "nothing",
        "partially",
        "hardly",
        "barely",
        "scarcely",
        "without",
        "almost",
        "nearly",
        "mostly",
    }
)
_NO_PHRASES = frozenset({("no", "longer"), ("no", "way")})
_FAIL_TO = frozenset({"fail", "fails", "failed"})
_REFUSE_TO = frozenset({"refuse", "refuses", "refused"})
_CLOSING_STEMS = frozenset(
    {
        "close",
        "closes",
        "closed",
        "fix",
        "fixes",
        "fixed",
        "resolve",
        "resolves",
        "resolved",
    }
)
_UNABLE_TO = ("unable", "to")
_UNLIKELY_TO = ("unlikely", "to")
_PENDING_TO = frozenset(
    {
        ("need", "to"),
        ("needs", "to"),
        ("needed", "to"),
        ("remain", "to"),
        ("remains", "to"),
        ("yet", "to"),
        ("plan", "to"),
        ("plans", "to"),
        ("planned", "to"),
    }
)
_PENDING_WORDS = frozenset({"todo"})
# Sequencing adverbs between a coordinating conj and an affirmative closer:
# `does not regress and then fixes #123` (#530 review).
_SEQUENCING_ADVERBS = frozenset(
    {
        "then",
        "subsequently",
        "afterwards",
        "afterward",
        "thereafter",
        "next",
        "finally",
        "eventually",
        "later",
    }
)
_WRAP_FILLERS = frozenset(
    {
        "really",
        "actually",
        "still",
        "even",
        "currently",
        "simply",
        "just",
        "quite",
        "also",
        "now",
        "fully",
        "completely",
        "utterly",
        "totally",
        "entirely",
        "absolutely",
        "outright",
        # Soft-wrap retention: `does not yet\nFix #123` must keep `not`
        # across the capitalized closer (#530 review).
        "yet",
    }
)
# Finite verbs that open an action-list predicate after a comma, so
# `does not change the API, adds tests, fixes #123` resets scope rather
# than treating `adds tests` as a paired aside (#530 review).
_ACTION_LIST_VERBS = frozenset(
    {
        "add",
        "adds",
        "added",
        "update",
        "updates",
        "updated",
        "introduce",
        "introduces",
        "introduced",
        "include",
        "includes",
        "included",
        "remove",
        "removes",
        "removed",
        "document",
        "documents",
        "documented",
        "improve",
        "improves",
        "improved",
        "refactor",
        "refactors",
        "refactored",
        "rename",
        "renames",
        "renamed",
        "implement",
        "implements",
        "implemented",
        "extend",
        "extends",
        "extended",
        "replace",
        "replaces",
        "replaced",
        "cover",
        "covers",
        "covered",
        "touch",
        "touches",
        "touched",
        "clean",
        "cleans",
        "cleaned",
    }
)
# Subject / relative / determiner openers for affirmative parentheticals:
# `does not change the API (it fixes #123)` / `(the patch fixes #123)`
# (#530 review).
_PAREN_AFFIRMATIVE_OPENERS = frozenset(
    {
        "it",
        "this",
        "that",
        "they",
        "we",
        "he",
        "she",
        "which",
        "who",
        "whom",
        "the",
        "a",
        "an",
        "our",
        "my",
        "their",
        "its",
    }
)
# Continuers that keep a verb-led paired comma as an aside rather than an
# action list: `does not, updates notwithstanding, fix #123` (#530 review).
_ASIDE_CONTINUERS = frozenset(
    {
        "notwithstanding",
        "aside",
        "however",
        "though",
        "although",
        "regardless",
    }
)
_ABBREV_WORDS = frozenset(
    {
        "e",
        "g",
        "i",
        "eg",
        "ie",
        "vs",
        "etc",
        "mr",
        "mrs",
        "ms",
        "dr",
        "prof",
        "inc",
        "ltd",
        "jr",
        "sr",
        "al",
        "cf",
    }
)
_REPO = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")


class PayloadError(ValueError):
    """The GitHub response did not have the requested stable shape."""


@dataclass(frozen=True)
class Finding:
    keyword: str
    reference: str
    source: str = ""


_LIST_ITEM = re.compile(r"[ \t]*(?:[-*+]|\d+\.)[ \t]")
_TRAILING_ISSUE_REF = re.compile(
    r"#\d+"
    r"|[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+#\d+"
    r"|https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/issues/\d+",
    re.IGNORECASE,
)


def _has_trailing_retraction(text: str, start: int) -> bool:
    """True when a closer is immediately retracted: ``Fixes #123 (not)``.

    GitHub still honours the leading directive; words after the reference
    were previously ignored (#530 review). Punctuation such as
    ``Fixes #123? No.`` must still reach the retraction (#530 review).
    Stop once that denial sentence completes so later prose cannot cancel
    it (`Fixes #123? No. It only adds tests.`) (#530 review).
    """

    content: list[tuple[str, int]] = []
    clause_end = len(text)
    for token in _SCAN_TOKEN.finditer(text, start):
        if token.lastgroup == "boundary":
            # Keep scanning past `?` until the retraction word arrives, then
            # stop at the next sentence boundary (#530 review).
            if content and content[0][0] in {"no", "not", "never"}:
                clause_end = token.start()
                break
            continue
        if token.lastgroup == "newline":
            nxt = _next_word(text, token.end())
            if nxt is None or nxt.group(0)[:1].isupper():
                clause_end = token.start()
                break
            continue
        if token.lastgroup == "comma":
            continue
        if token.lastgroup == "paren":
            continue
        if token.lastgroup != "word":
            continue
        word = token.group(0).casefold().replace("’", "'")
        if word in _WRAP_FILLERS:
            continue
        content.append((word, token.end()))
        if len(content) > 8:
            break
    if not content:
        return False
    first, first_end = content[0]
    if first not in {"no", "not", "never"}:
        return False
    if first == "not" and len(content) > 1 and content[1][0] in {
        "only",
        "just",
        "merely",
    }:
        return False
    rest = text[first_end:clause_end]
    if _TRAILING_ISSUE_REF.search(rest):
        return False
    rest_words = [word for word, _end in content[1:]]
    if first == "no":
        # Bare `No.` / `no longer` / `no way` / `no yet` stay retractions.
        # Same-sentence denials (`No, it doesn't.`) also retract (#530).
        if not rest_words:
            return True
        if rest_words[0] in {"longer", "way", "yet"}:
            return True
        return _has_negation(tuple(rest_words))
    if first == "not" and rest_words == ["yet"]:
        return True
    return not rest_words


def _strip_double_negatives(counted: tuple[str, ...]) -> tuple[str, ...]:
    """Drop only a governed ``not/never … fail to`` / ``not … unlikely`` phrase.

    Other negatives in the same clause must still deny the closer
    (`does not help and is unlikely to fix #123`) (#530 review).
    ``never fails to fix`` is affirmative (#530 review).
    """

    skip: set[int] = set()
    i = 0
    while i < len(counted):
        word = counted[i]
        is_not = (
            word == "not"
            or word == "never"
            or word.endswith("n't")
            or word in {"doesnt", "didnt", "dont", "isnt"}
        )
        if not is_not or i in skip:
            i += 1
            continue
        j = i + 1
        while j < len(counted) and counted[j] in _WRAP_FILLERS:
            j += 1
        # `never unlikely` is not a governed double negative; only `not`
        # / n't forms invert inability adjectives (#530).
        if (
            word != "never"
            and j < len(counted)
            and counted[j]
            in {
                "unlikely",
                "unable",
                "impossible",
                "insufficient",
                "inadequate",
            }
        ):
            skip.update(range(i, j + 1))
            i = j + 1
            continue
        if j < len(counted) and counted[j] in _FAIL_TO:
            k = j + 1
            while k < len(counted) and counted[k] in _WRAP_FILLERS:
                k += 1
            if k < len(counted) and counted[k] == "to":
                skip.update(range(i, k + 1))
                i = k + 1
                continue
        i += 1
    return tuple(
        word for index, word in enumerate(counted) if index not in skip
    )


def _has_negation(words: tuple[str, ...]) -> bool:
    skip = set()
    for index, (first, second) in enumerate(zip(words, words[1:])):
        additive = second in {"only", "just", "merely"} and (
            first == "not"
            or first.endswith("n't")
            or first in {"doesnt", "didnt", "dont", "isnt"}
        )
        if additive:
            skip.add(index)
            skip.add(index + 1)
    counted = tuple(word for index, word in enumerate(words) if index not in skip)
    counted = _strip_double_negatives(counted)
    pairs = tuple(zip(counted, counted[1:]))
    if any(
        a in {"anything", "everything", "anyone", "anybody"} and b == "but"
        for a, b in pairs
    ):
        return True
    if any(
        a in {"anything", "everything", "anyone", "anybody"} and b == "except"
        for a, b in pairs
    ):
        return True
    if any(a == "other" and b == "than" for a, b in pairs):
        return True
    if any(a == "except" and b in _CLOSING_STEMS for a, b in pairs):
        return True
    if any(
        word in _NEGATION_WORDS
        or word.endswith("n't")
        for word in counted
    ):
        return True
    # Standalone `no` / `TODO` only when they govern the closer:
    # `There is no fix #123`, `There is no API fix #123`, `TODO: fix #123`.
    # `No API changes, fixes #123` and `Removes the TODO and fixes #123`
    # stay affirmative because clause resets drop `no` before the closer
    # (#530 review).
    trailing = len(counted) - 1
    while trailing >= 0 and counted[trailing] in _WRAP_FILLERS:
        trailing -= 1
    if trailing >= 0 and counted[trailing] in {"no"} | _PENDING_WORDS:
        return True
    leading = 0
    while leading < len(counted) and counted[leading] in _WRAP_FILLERS:
        leading += 1
    if leading < len(counted) and counted[leading] == "no":
        return True
    if "no" in counted:
        idx = max(i for i, word in enumerate(counted) if word == "no")
        rest = counted[idx + 1 :]
        if not rest or rest[0] not in {"longer", "way"}:
            return True
    if any(pair in _NO_PHRASES for pair in pairs):
        return True
    if any(a in _FAIL_TO and b == "to" for a, b in pairs):
        return True
    # Skip arbitrary modifiers until `to` or a real clause/closer boundary —
    # no fixed token cap (`fails consistently even now in every environment
    # to fix #123`) (#530 review).
    _INABILITY_TO = frozenset(
        {
            "unable",
            "unlikely",
            "impossible",
            "insufficient",
            "inadequate",
        }
    )
    for starter in (_FAIL_TO, _REFUSE_TO, _INABILITY_TO):
        for i, word in enumerate(counted):
            if word not in starter:
                continue
            for j in range(i + 1, len(counted)):
                if counted[j] == "to":
                    return True
                if counted[j] in _CLOSING_STEMS:
                    break
                if counted[j] in _ADVERSATIVE | {"and", "or", "but"}:
                    break
    if _UNABLE_TO in pairs:
        return True
    if _UNLIKELY_TO in pairs:
        return True
    if any(pair in _PENDING_TO for pair in pairs):
        return True
    # `still needs more work to fix #123` — object phrases may sit between
    # the pending verb and `to`. Keep scanning until a real clause/closer
    # boundary, not a fixed token distance (#530 review).
    _PENDING_STARTERS = frozenset(
        {
            "need",
            "needs",
            "needed",
            "remain",
            "remains",
            "yet",
            "plan",
            "plans",
            "planned",
        }
    )
    for i, word in enumerate(counted):
        if word not in _PENDING_STARTERS:
            continue
        for j in range(i + 1, len(counted)):
            if counted[j] == "to":
                return True
            if counted[j] in _CLOSING_STEMS:
                break
            if counted[j] in _ADVERSATIVE:
                break
    return any(a == "rather" and b == "than" for a, b in pairs)


def _is_abbrev_word(word: str) -> bool:
    return word in _ABBREV_WORDS


def _period_is_abbreviation(
    token: re.Match[str], prev_word: str, text: str | None = None
) -> bool:
    """`e.g.` / `i.e.` periods are not clause boundaries (#530 review).

    An abbreviation can also end a sentence (`Acme Inc. Fixes #123`); a
    following capitalized word is treated as a real boundary.
    """

    if token.group(0) != "." or not _is_abbrev_word(prev_word):
        return False
    if text is not None:
        nxt = _next_word(text, token.end())
        if nxt is not None and nxt.group(0)[:1].isupper():
            return False
    return True


def _next_word(text: str, start: int) -> re.Match[str] | None:
    for token in _SCAN_TOKEN.finditer(text, start):
        if token.lastgroup == "word":
            return token
        if token.lastgroup == "boundary":
            return None
    return None


def _inner_is_parenthetical_aside(inner_words: tuple[str, ...]) -> bool:
    """True for paired asides that must keep negation.

    Known starters (`in fact`, prepositions) and abbreviations are asides.
    Unrecognized paired commas (`never, ever,` / `does not, frankly speaking,`)
    also keep negation — resetting on every unknown aside would let GitHub
    honor the closer (#530 review). Coordinating `, and` / `, or` / `, but`
    stays a clause boundary. Finite action-list verbs (`adds tests`) are
    ordinary predicates and must reset (#530 review).
    """

    if not inner_words or inner_words[0] in {"and", "or", "but"}:
        return False
    if inner_words[0] in _ACTION_LIST_VERBS:
        # `updates notwithstanding` is still a paired aside (#530 review).
        if any(w in _ASIDE_CONTINUERS for w in inner_words[1:4]):
            return True
        return False
    return True


def _subordinate_introduces_affirmative_closer(
    text: str, start: int, closings: dict[int, re.Match[str]]
) -> bool:
    """True for `because it fixes #123`; false for unpunctuated modifiers.

    ``This does not while running offline fix #123`` keeps negation;
    ``because it fixes`` resets (#530 review).
    """

    leading: list[str] = []
    for token in _SCAN_TOKEN.finditer(text, start):
        if token.lastgroup in {"boundary", "newline"}:
            return False
        if token.lastgroup == "comma":
            continue
        if token.lastgroup == "paren":
            continue
        if token.lastgroup != "word":
            continue
        word = token.group(0).casefold().replace("’", "'")
        if token.start() in closings:
            if _has_negation(tuple(leading)):
                return False
            content = [
                w
                for w in leading
                if w not in _WRAP_FILLERS | _SEQUENCING_ADVERBS
            ]
            if not content:
                return True
            return content[0] in _PAREN_AFFIRMATIVE_OPENERS
        leading.append(word)
        if len(leading) > 10:
            return False
    return False


def _coord_introduces_affirmative_closer(
    text: str, start: int, closings: dict[int, re.Match[str]]
) -> bool:
    """True for ``and adds tests that fix #123`` style coordination (#530)."""

    leading: list[str] = []
    for token in _SCAN_TOKEN.finditer(text, start):
        if token.lastgroup in {"boundary", "newline"}:
            return False
        if token.lastgroup != "word":
            continue
        word = token.group(0).casefold().replace("’", "'")
        if token.start() in closings:
            if _has_negation(tuple(leading)):
                return False
            # Shared infinitive under pending/fail (`needs … and review to
            # fix #123`) must keep the earlier denial (#530).
            if "to" in leading:
                return False
            return True
        leading.append(word)
        if len(leading) > 12:
            return False
    return False


def _paren_starts_affirmative_predicate(
    text: str, start: int, closings: dict[int, re.Match[str]]
) -> bool:
    """True for `(it fixes #123)` / `(also fixes #123)` style asides.

    Direct `(fixes #123)` is handled by the caller. Keep negation when the
    parenthesis itself is negated (`(does not fix #123)`).
    """

    leading: list[str] = []
    for token in _SCAN_TOKEN.finditer(text, start):
        if token.lastgroup == "paren" and token.group(0) == ")":
            return False
        if token.lastgroup == "boundary":
            return False
        if token.lastgroup != "word":
            continue
        word = token.group(0).casefold().replace("’", "'")
        if token.start() in closings:
            if _has_negation(tuple(leading)):
                return False
            content = [
                w
                for w in leading
                if w not in _WRAP_FILLERS | _SEQUENCING_ADVERBS
            ]
            if not content:
                return True
            return content[0] in _PAREN_AFFIRMATIVE_OPENERS
        leading.append(word)
        if len(leading) > 8:
            return False
    return False


def _comma_opens_parenthetical(
    text: str, start: int, closings: dict[int, re.Match[str]]
) -> bool:
    """Paired aside commas keep negation; action-list commas do not."""

    inner: list[str] = []
    for token in _SCAN_TOKEN.finditer(text, start):
        if token.lastgroup == "boundary":
            prev = inner[-1] if inner else ""
            if _period_is_abbreviation(token, prev, text):
                continue
            return False
        if token.lastgroup == "newline":
            continue
        if token.lastgroup == "comma":
            return _inner_is_parenthetical_aside(tuple(inner))
        if token.lastgroup != "word":
            continue
        if token.start() in closings:
            return _inner_is_parenthetical_aside(tuple(inner))
        inner.append(token.group(0).casefold().replace("’", "'"))
        if len(inner) > 8:
            return False
    return False


def _dash_opens_parenthetical(
    text: str, start: int, closings: dict[int, re.Match[str]]
) -> bool:
    """True only for paired dash asides; clause dashes reset (#530 review).

    ``does not — in fact — close #123`` keeps negation. A single dash before
    a new predicate (``does not change the API — it fixes #123``) does not.
    """

    inner: list[str] = []
    for token in _SCAN_TOKEN.finditer(text, start):
        if token.lastgroup == "boundary":
            return False
        if token.lastgroup == "newline":
            continue
        if token.lastgroup == "dash":
            return _inner_is_parenthetical_aside(tuple(inner))
        if token.lastgroup != "word":
            continue
        if token.start() in closings:
            # Closer without a closing dash → clause separator, not aside.
            return False
        inner.append(token.group(0).casefold().replace("’", "'"))
        if len(inner) > 8:
            return False
    return False


def _however_starts_new_clause(
    text: str, token: re.Match[str], closings: dict[int, re.Match[str]]
) -> bool:
    """`however` is a clause boundary unless it is parenthetical before a closing verb.

    ``This does not, however, close #123`` keeps ``not``.
    ``This does not change the API, however it fixes #123`` starts a new clause.
    """
    nxt = _next_word(text, token.end())
    return nxt is None or nxt.start() not in closings


def find_negated_closing_keywords(text: str) -> list[Finding]:
    closings = {
        match.start("keyword"): match for match in _CLOSING.finditer(text)
    }
    findings = []
    # Retain every token until an actual clause/list/adversative boundary —
    # a fixed-length window would drop early negation under long modifiers
    # (`does not … actually fix #123`) and miss GitHub closers (#530 review).
    words: deque[str] = deque()
    prev_word = ""
    in_parenthetical = False
    negated_list_open = False
    for token in _SCAN_TOKEN.finditer(text):
        if token.lastgroup == "boundary":
            if _period_is_abbreviation(token, prev_word, text):
                continue
            words.clear()
            prev_word = ""
            in_parenthetical = False
            negated_list_open = False
            continue
        if token.lastgroup == "newline":
            # Blank Markdown paragraph (`\n\n`) ends the prior clause so
            # `No API changes\n\nfixes #123` stays affirmative (#530 review).
            # Keep scope when negation still governs — GitHub joins commit
            # subject/body with `\n\n` (`This does not fully\n\nfix #123`).
            if text[token.end() : token.end() + 1] == "\n":
                last = ""
                for prior in reversed(words):
                    if prior not in _WRAP_FILLERS:
                        last = prior
                        break
                if last and (
                    last in _NEGATION_WORDS
                    or last in _PENDING_WORDS
                    or last in {"to", "no"}
                    or last.endswith("n't")
                    or last.endswith("nt")
                ):
                    continue
                words.clear()
                prev_word = ""
                in_parenthetical = False
                negated_list_open = False
                continue
            if _LIST_ITEM.match(text, token.end()):
                # `This does not:\n- fix #123` — open negation continues into
                # sibling bullets. A completed prior bullet
                # (`- Does not change the API`) must not poison the next
                # affirmative item (#530 review).
                tail = [w for w in words if w not in _WRAP_FILLERS]
                neg_idxs = [
                    i
                    for i, prior in enumerate(tail)
                    if (
                        prior in _NEGATION_WORDS
                        or prior in _PENDING_WORDS
                        or prior == "to"
                        or prior.endswith("n't")
                        or prior.endswith("nt")
                    )
                ]
                open_neg = bool(neg_idxs) and not tail[neg_idxs[-1] + 1 :]
                if open_neg:
                    negated_list_open = True
                    continue
                if negated_list_open:
                    continue
                words.clear()
                prev_word = ""
                in_parenthetical = False
                continue
            last = ""
            for prior in reversed(words):
                if prior not in _WRAP_FILLERS:
                    last = prior
                    break
            # Soft-wrapped closers keep governing negation, including one
            # unlisted modifier after `not` (`does not adequately\nFix #123`).
            # A completed predicate after `not` (`does not change X\nCloses`)
            # still clears (#530 review).
            nxt = _next_word(text, token.end())
            tail = [w for w in words if w not in _WRAP_FILLERS]
            keep_wrap = False
            if tail:
                tip = tail[-1]
                if (
                    tip in _NEGATION_WORDS
                    or tip in _PENDING_WORDS
                    or tip in _FAIL_TO
                    or tip in _REFUSE_TO
                    or tip in {"to", "unable", "unlikely", "impossible", "insufficient", "inadequate"}
                    or tip.endswith("n't")
                    or tip.endswith("nt")
                ):
                    keep_wrap = True
                elif len(tail) >= 2:
                    prev = tail[-2]
                    if (
                        prev in _NEGATION_WORDS
                        or prev.endswith("n't")
                        or prev.endswith("nt")
                    ):
                        keep_wrap = True
            if keep_wrap:
                continue
            if last and (
                last in _NEGATION_WORDS
                or last in _PENDING_WORDS
                or last in {"to", "no"}
                or last.endswith("n't")
                or last.endswith("nt")
            ):
                continue
            if last in {"and", "or"} and nxt is not None and nxt.start() in closings:
                words.clear()
                prev_word = ""
                in_parenthetical = False
                continue
            if nxt is None or nxt.group(0)[:1].isupper():
                words.clear()
                prev_word = ""
                in_parenthetical = False
            continue
        if token.lastgroup == "paren":
            if token.group(0) == "(":
                nxt = _next_word(text, token.end())
                if nxt is not None and nxt.start() in closings:
                    # Affirmative aside after unrelated prose:
                    # `does not change the API (fixes #123)`.
                    # Keep only when negation directly governs the paren:
                    # `does not (fix #123)` / `cannot (close #123)`.
                    last = ""
                    for prior in reversed(words):
                        if prior not in _WRAP_FILLERS:
                            last = prior
                            break
                    governed = (
                        last in _NEGATION_WORDS
                        or last.endswith("n't")
                        or last in {"no"}
                    )
                    if not governed:
                        words.clear()
                        prev_word = ""
                        in_parenthetical = False
                elif _paren_starts_affirmative_predicate(
                    text, token.end(), closings
                ):
                    # `(it fixes #123)` / `(also fixes #123)` start a new
                    # affirmative predicate (#530 review).
                    words.clear()
                    prev_word = ""
                    in_parenthetical = False
            continue
        if token.lastgroup == "dash":
            # Clause-separating dashes reset scope; only paired dash asides
            # keep negation (`does not — in fact — close`) (#530 review).
            if in_parenthetical:
                in_parenthetical = False
                continue
            if _dash_opens_parenthetical(text, token.end(), closings):
                in_parenthetical = True
            else:
                words.clear()
                prev_word = ""
                in_parenthetical = False
            continue
        if token.lastgroup == "comma":
            nxt = _next_word(text, token.end())
            nxt_word = "" if nxt is None else nxt.group(0).casefold()
            if in_parenthetical:
                in_parenthetical = False
                if nxt_word in {"and", "or", "but"}:
                    words.clear()
                    prev_word = ""
                continue
            if nxt is not None and nxt.start() in closings:
                words.clear()
                prev_word = ""
                in_parenthetical = False
            elif nxt_word in {"and", "or", "but"}:
                words.clear()
                prev_word = ""
                in_parenthetical = False
            elif _comma_opens_parenthetical(text, token.end(), closings):
                in_parenthetical = True
            else:
                words.clear()
                prev_word = ""
                in_parenthetical = False
            continue

        word = token.group(0).casefold().replace("’", "'")
        if word == "but" and prev_word in {
            "anything",
            "everything",
            "nothing",
            "anyone",
            "anybody",
        }:
            # `anything but fix #123` is a denial, not a new clause
            # (#530 review).
            words.append(word)
            prev_word = word
            continue
        if word in _ADVERSATIVE or (
            word == "however"
            and _however_starts_new_clause(text, token, closings)
        ):
            # Independent clause after "but" / clause-initial "however"
            # does not inherit negation from the preceding clause.
            words.clear()
            prev_word = ""
            continue
        if word in _SUBORDINATE_RESET:
            # `because of …` is a prepositional modifier, not a new predicate
            # (`This cannot because of API limitations fix #123`) (#530).
            nxt = _next_word(text, token.end())
            nxt_word = (
                ""
                if nxt is None
                else nxt.group(0).casefold().replace("’", "'")
            )
            if word == "because" and nxt_word == "of":
                words.append(word)
                prev_word = word
                continue
            # Inside a paired-comma aside, subordinate words are modifiers
            # (`cannot, because of API limitations, fix #123`) (#530).
            if in_parenthetical:
                words.append(word)
                prev_word = word
                continue
            # Reset only when the subordinate introduces an independent
            # affirmative closer (`because it fixes`). Unpunctuated
            # modifiers (`does not while running offline fix`) keep
            # negation (#530 review).
            if _subordinate_introduces_affirmative_closer(
                text, token.end(), closings
            ):
                words.clear()
                prev_word = ""
            else:
                words.append(word)
                prev_word = word
            continue
        if word in {"and", "or"}:
            nxt = _next_word(text, token.end())
            while nxt is not None and (
                nxt.group(0).casefold().replace("’", "'")
                in _WRAP_FILLERS | _SEQUENCING_ADVERBS
            ):
                nxt = _next_word(text, nxt.end())
            last = ""
            for prior in reversed(words):
                if prior not in _WRAP_FILLERS:
                    last = prior
                    break
            # `does not regress X and fixes #123` is a new predicate.
            # `does not close or fix #123` keeps negation because the
            # previous content word is itself a closer (#530 review).
            # Bare infinitive under a shared auxiliary also keeps it:
            # `does not address the bug and fix #123`.
            # Sequencing adverbs do not preserve the prior `not`:
            # `does not regress and then fixes #123` (#530 review).
            # Coordinated predicates with leading words also reset:
            # `doesn't change the API and adds tests that fix #123`
            # (#530 review).
            if (
                nxt is not None
                and nxt.start() in closings
                and last not in _CLOSING_STEMS
            ):
                closer = nxt.group(0).casefold()
                if closer in {"fix", "close", "resolve"} and _has_negation(
                    tuple(words)
                ):
                    pass
                else:
                    words.clear()
                    prev_word = ""
                    continue
            elif word == "and" and _coord_introduces_affirmative_closer(
                text, token.end(), closings
            ):
                words.clear()
                prev_word = ""
                continue

        match = closings.get(token.start())
        if match is not None and (
            _has_negation(tuple(words))
            or _has_trailing_retraction(text, match.end())
        ):
            findings.append(
                Finding(
                    keyword=match.group("keyword").casefold(),
                    reference=match.group("reference"),
                )
            )
        words.append(word)
        prev_word = word
    return findings


def _required_text(mapping: dict, key: str, *, nullable: bool = False) -> str:
    if key not in mapping:
        raise PayloadError(f"missing {key}")
    value = mapping[key]
    if nullable and value is None:
        return ""
    if not isinstance(value, str):
        raise PayloadError(f"invalid {key}")
    return value


def _commit_message(headline: str, body: str) -> str:
    """GitHub closing keywords see the whole commit message, not two fields."""

    headline = headline.rstrip()
    body = body.lstrip("\n")
    if headline and body:
        return f"{headline}\n\n{body}"
    return headline or body


def find_payload_findings(payload: object) -> list[Finding]:
    if not isinstance(payload, dict):
        raise PayloadError("response is not an object")
    sources = [
        ("pr.title", _required_text(payload, "title")),
        ("pr.body", _required_text(payload, "body", nullable=True)),
    ]
    commits = payload.get("commits")
    if not isinstance(commits, list) or not commits:
        raise PayloadError("invalid commits")
    for index, commit in enumerate(commits, 1):
        if not isinstance(commit, dict):
            raise PayloadError("invalid commit")
        sources.append(
            (
                f"commit[{index}]",
                _commit_message(
                    _required_text(commit, "messageHeadline"),
                    _required_text(commit, "messageBody", nullable=True),
                ),
            )
        )

    findings = []
    for source, text in sources:
        findings.extend(
            Finding(finding.keyword, finding.reference, source)
            for finding in find_negated_closing_keywords(text)
        )
    return findings


_PR_COMMITS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      title
      body
      commits(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { commit { messageHeadline messageBody } }
      }
    }
  }
}
"""


def _fetch_pr_payload(pr: str, repo: str) -> dict:
    """Title, body, and every source commit. `gh pr view --json commits` is
    capped at 100; GraphQL pagination is not (#513 review)."""

    owner, name = repo.split("/", 1)
    cursor = None
    title = None
    body = None
    commits: list[dict] = []
    while True:
        args = [
            "gh",
            "api",
            "graphql",
            "-f",
            f"query={_PR_COMMITS_QUERY}",
            "-F",
            f"owner={owner}",
            "-F",
            f"name={name}",
            "-F",
            f"number={pr}",
        ]
        if cursor:
            args.extend(["-F", f"cursor={cursor}"])
        try:
            proc = subprocess.run(args, capture_output=True, text=True)
        except OSError as exc:
            raise PayloadError("could not run gh") from exc
        if proc.returncode != 0:
            raise PayloadError("gh could not inspect PR metadata")
        try:
            parsed = json.loads(proc.stdout)
        except json.JSONDecodeError as exc:
            raise PayloadError("gh returned malformed PR metadata") from exc
        try:
            pr_node = parsed["data"]["repository"]["pullRequest"]
        except (KeyError, TypeError) as exc:
            raise PayloadError("gh returned malformed PR metadata") from exc
        if not isinstance(pr_node, dict):
            raise PayloadError("gh returned malformed PR metadata")
        # Title/body can change between pages; the last page is current.
        title = pr_node.get("title")
        body = pr_node.get("body")
        connection = pr_node.get("commits")
        if not isinstance(connection, dict):
            raise PayloadError("invalid commits")
        nodes = connection.get("nodes")
        if not isinstance(nodes, list):
            raise PayloadError("invalid commits")
        for node in nodes:
            if not isinstance(node, dict) or not isinstance(node.get("commit"), dict):
                raise PayloadError("invalid commit")
            commit = node["commit"]
            commits.append(
                {
                    "messageHeadline": commit.get("messageHeadline"),
                    "messageBody": commit.get("messageBody"),
                }
            )
        page = connection.get("pageInfo")
        if not isinstance(page, dict):
            raise PayloadError("invalid commits")
        if not page.get("hasNextPage"):
            break
        cursor = page.get("endCursor")
        if not isinstance(cursor, str) or not cursor:
            raise PayloadError("invalid commits")
    return {"title": title, "body": body, "commits": commits}


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pr", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument(
        "--print-digest",
        action="store_true",
        help="Print a title/body digest so merge-pr.sh can detect late edits",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if not args.pr.isdecimal() or not _REPO.fullmatch(args.repo):
        print("error: invalid PR number or repository name", file=sys.stderr)
        return 2

    try:
        payload = _fetch_pr_payload(args.pr, args.repo)
        findings = find_payload_findings(payload)
    except PayloadError:
        print("error: gh could not inspect PR metadata", file=sys.stderr)
        return 2

    if findings:
        for finding in findings[:MAX_REPORTED_FINDINGS]:
            print(
                f"error: negated closing keyword in {finding.source}: "
                f"{finding.keyword} {finding.reference}",
                file=sys.stderr,
            )
        omitted = len(findings) - MAX_REPORTED_FINDINGS
        if omitted > 0:
            print(
                f"error: {omitted} additional finding(s) omitted",
                file=sys.stderr,
            )
        print(
            "error: rephrase the scope statement as 'Leaves #N open' or "
            "'#N is out of scope' before merging",
            file=sys.stderr,
        )
        return 1

    print("ok: no negated GitHub closing keywords found")
    if args.print_digest:
        blob = f"{payload['title']}\n{payload['body']}".encode()
        print(f"digest: {hashlib.sha256(blob).hexdigest()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

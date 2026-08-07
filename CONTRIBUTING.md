# Contributing to Medley

Thanks for helping improve [ImL1s/medley](https://github.com/ImL1s/medley).
This fork accepts external issues and pull requests.

## Before you open a PR

- Read [`FORK.md`](FORK.md) for branch roles and sync rules.
- Target **`providers`**. Do not target `main` (it is a pristine upstream mirror).
- Keep changes focused and explain user impact in the PR body.
- Run the checks relevant to your change (tests, lint, docs examples) before opening.

## What to open where

- Bugs and feature requests: <https://github.com/ImL1s/medley/issues>
- Code changes: pull requests to `providers`
- Upstream sync work: follow `scripts/sync-upstream.sh` + [`FORK.md`](FORK.md)

## Security-sensitive changes

Do not include real credentials, API keys, or tokens in issues, commits,
examples, or logs. For vulnerability reporting, use the process in
[`SECURITY.md`](SECURITY.md).

## License

By contributing, you agree that your contributions are licensed under the
Apache License, Version 2.0 (see [`LICENSE`](LICENSE)).

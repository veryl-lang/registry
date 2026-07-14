# Veryl registry

The [Veryl registry](https://registry.veryl-lang.org) is an opt-in directory of
published Veryl projects and their generated documentation.

It is **not** a package host. Veryl dependencies are decentralized: each project's own
git repository is the source of truth, and `veryl publish` records
`version -> git revision` in its `Veryl.pub`. This repository is just a thin index of
*which repositories* to watch — a crawler reads each one's `Veryl.pub`, builds docs with
`veryl doc`, and publishes the gallery and docs site to `gh-pages`.

## Registering a project

Registration is opt-in and only ever documents a repository whose author asked for it:

- `veryl publish` with `[publish] register = true` (or answer the one-time prompt), or
- `veryl register` at any time.

The entry lands as a pull request for a maintainer to merge. Registration does not push,
so run `git push` to make the published revision visible; docs then appear at
`https://registry.veryl-lang.org/<owner>/<repo>/<project>/<version>/` after the next crawl.

## Opting out and removal

- **Opt out:** set `[publish] register = false` in your `Veryl.toml`. A third party then
  cannot register the project on your behalf.
- **Remove or dispute:** open a pull request removing `registry/<owner>/<repo>.json` (or
  setting its `status` to `yanked`). Docs for such entries are dropped on the next crawl.

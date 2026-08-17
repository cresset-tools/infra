# Cresset View

A jj-native web viewer for the Cresset monorepo. The service reads changes,
exact commit versions, bookmarks, trees, and conflicts through `jj-lib`. Its
public API and interface intentionally contain no Git concepts.

**The repository is read-only; review is not.** Nothing here ever writes to the
repository — it does not snapshot the working copy, create commits, or move
bookmarks. Review comments and approvals are the only things the service writes,
and they go to a separate SQLite database given by `--review-db`, never into git.
An instance started without that flag serves the review queue and its diffs and
refuses writes with an explanation.

`--approvals-file` projects approvals to a flat file for the canonical
repository's push gate (`hosts/internal/hooks/update`) to read. The gate runs as
another user inside `receive-pack`, so it reads a file rather than this database
— see `src/approvals.rs` for why. Without the flag, approvals are still recorded
and the UI says plainly that nothing enforces them.

Authentication is not implemented in the application. Production deployment
binds it to loopback behind nginx and Authentik forward-auth. Because that means
every request from the reviewer's browser is authenticated, writes additionally
refuse an explicit cross-site `Sec-Fetch-Site`.

## Development

Build the frontend, then run the service against a jj workspace:

```sh
cd web
npm install
npm run build
npm test          # graph layout, comment anchoring, thread placement; also run by the Nix build
cd ..
cargo run -- --repository /home/jelle/cresset --dev-identity you
```

In production an Authentik proxy asserts who the caller is via the `x-authentik-username`
header, and the service refuses requests without one. `--dev-identity` stands in for that
proxy locally: requests with no identity header are served as the given user. It only works
on a loopback listener — with anything else the server refuses to start.

`npm test` is a plain script rather than a test runner: this Node build cannot start
`node:test`, and `node:assert` would pull `@types/node` into the app's typecheck. It runs
in the Nix sandbox too (`package.nix`, `checkPhase`), so a broken layout fails the build
rather than only the laptop it was written on.

Comment anchoring lives in the browser (`web/src/anchor.ts`, `web/src/threads.ts`) rather
than the server. A comment is placed by searching the patch set being read for the line it
was written against, so the code doing the searching should be where the file content
already is — and the store can then keep the anchor as opaque text it never interprets.

Open `http://127.0.0.1:8080`. The viewer refuses repositories with divergent
operation heads instead of reconciling them and never snapshots the working
copy.

`--check` opens the repository, verifies that it has exactly one operation
head, prints that operation ID, and exits. The publication script uses this
before atomically activating a repository snapshot.

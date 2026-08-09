# Cresset View

Read-only, jj-native web viewer for the Cresset monorepo. The service reads
changes, exact commit versions, bookmarks, trees, and conflicts through
`jj-lib`. Its public API and interface intentionally contain no Git concepts.

Authentication is not implemented in the application. Production deployment
binds it to loopback behind nginx and Authentik forward-auth.

## Development

Build the frontend, then run the service against a jj workspace:

```sh
cd web
npm install
npm run build
npm test          # revision graph layout checks; also run by the Nix build
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

Open `http://127.0.0.1:8080`. The viewer refuses repositories with divergent
operation heads instead of reconciling them and never snapshots the working
copy.

`--check` opens the repository, verifies that it has exactly one operation
head, prints that operation ID, and exits. The publication script uses this
before atomically activating a repository snapshot.

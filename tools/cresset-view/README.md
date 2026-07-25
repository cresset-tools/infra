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
cd ..
cargo run -- --repository /home/jelle/cresset
```

Open `http://127.0.0.1:8080`. The viewer refuses repositories with divergent
operation heads instead of reconciling them and never snapshots the working
copy.

`--check` opens the repository, verifies that it has exactly one operation
head, prints that operation ID, and exits. The publication script uses this
before atomically activating a repository snapshot.

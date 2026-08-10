// Checks for comment anchoring across patch sets.
//
// The corpus is REAL: excerpts of this repository's own history, not invented before/after
// pairs. Invented cases are the failure mode here — it is easy to write a relocation test
// that agrees with the algorithm you just wrote, and the thing that matters is whether it
// survives edits people actually make.
//
// Dependency-free for the same reasons as graph.test.ts: this Node build cannot start
// `node:test`, and `node:assert` would pull @types/node into the app's typecheck. Run with
// `npm test`.
import { captureAnchor, relocate, type Side } from './anchor';

let failures = 0;

function check(name: string, body: () => void) {
  try {
    body();
    console.log(`ok   ${name}`);
  } catch (error) {
    failures += 1;
    console.error(`FAIL ${name}`);
    console.error(`     ${error instanceof Error ? error.message : String(error)}`);
  }
}

function assertEqual(actual: unknown, expected: unknown, message: string) {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new Error(`${message}\n     actual:   ${a}\n     expected: ${b}`);
}

/// `String.raw` because the corpus contains `\n` inside source strings, which a normal
/// template literal would turn into an actual newline and quietly change the fixture.
const trim = (text: string) => text.replace(/^\n/, '');

// ---------------------------------------------------------------------------
// Corpus 1 — web/src/diff-worker.ts, before and after the commit that stopped the worker
// discarding conflict content (41854d8b1 -> 3f971a11e). A twelve-line comment was inserted
// and the `const notice` line rewritten, so everything below it really moved by 7 lines.
// ---------------------------------------------------------------------------

const WORKER_BEFORE = trim(String.raw`
}

self.addEventListener('message', (message: MessageEvent<DiffWorkerRequest>) => {
  const { id, file } = message.data;
  try {
    const notice = file.conflicted
      ? 'This path contains an unresolved jj conflict.\n'
      : file.binary
        ? 'Binary or oversized content is not rendered.\n'
        : null;
    const fileDiff = parseDiffFromFile(
      { name: file.path, contents: notice == null ? file.before ?? '' : '' },
      { name: file.path, contents: notice ?? file.after ?? '' },
    );
`);

const WORKER_AFTER = trim(String.raw`
}

self.addEventListener('message', (message: MessageEvent<DiffWorkerRequest>) => {
  const { id, file } = message.data;
  try {
    // A conflicted file is diffed like any other.
    //
    // This used to substitute 'This path contains an unresolved jj conflict.' for the content
    // and diff THAT, so the Changes view discarded whatever the server sent and rendered the
    // same dead end the file view used to. Both sides now arrive as jj's marker text when
    // unresolved, so the ordinary diff shows the parent's content against the conflict — which
    // is what someone opening Changes came to see.
    //
    // Binary keeps its notice: there is genuinely nothing to diff, and the server says so by
    // sending no text. That includes a conflict with no textual form at all, such as a file
    // against a directory.
    const notice = file.binary ? 'Binary or oversized content is not rendered.\n' : null;
    const fileDiff = parseDiffFromFile(
      { name: file.path, contents: notice == null ? file.before ?? '' : '' },
      { name: file.path, contents: notice ?? file.after ?? '' },
    );
`);

const side: Side = 'additions';
const at = (contents: string, line: number) =>
  captureAnchor('web/src/diff-worker.ts', side, contents, line);

check('an untouched line keeps its place', () => {
  const anchor = at(WORKER_BEFORE, 4);
  assertEqual(WORKER_BEFORE.split('\n')[3], '  const { id, file } = message.data;', 'fixture');
  assertEqual(
    relocate(anchor, WORKER_BEFORE),
    { line: 4, confidence: 'exact' },
    'relocating into the same content must be exact',
  );
});

check('a line pushed down by an insertion is followed', () => {
  // `const fileDiff = parseDiffFromFile(` is line 11 before and line 18 after — a real
  // 7-line shift caused by the comment block that was inserted above it.
  const anchor = at(WORKER_BEFORE, 11);
  assertEqual(
    WORKER_BEFORE.split('\n')[10],
    '    const fileDiff = parseDiffFromFile(',
    'fixture: the anchored line',
  );
  assertEqual(
    relocate(anchor, WORKER_AFTER),
    { line: 18, confidence: 'moved' },
    'the comment must follow the line it was written against',
  );
});

check('a line that was edited away goes stale rather than guessing', () => {
  // `const notice = file.conflicted` does not exist after the change. There IS a line
  // mentioning `const notice` nearby, and the wrong answer is to point at it.
  const anchor = at(WORKER_BEFORE, 6);
  assertEqual(
    WORKER_BEFORE.split('\n')[5],
    '    const notice = file.conflicted',
    'fixture: the anchored line',
  );
  assertEqual(
    relocate(anchor, WORKER_AFTER),
    { line: null, confidence: 'stale' },
    'an edited line must be reported stale, not relocated onto its replacement',
  );
});

check('context decides between two identical lines', () => {
  // These two really are duplicates in the file, differing only in surrounding text:
  //   { name: file.path, contents: notice == null ? file.before ?? '' : '' },
  //   { name: file.path, contents: notice ?? file.after ?? '' },
  // Anchoring the first must not land on the second.
  const anchor = at(WORKER_BEFORE, 12);
  const moved = relocate(anchor, WORKER_AFTER);
  assertEqual(moved, { line: 19, confidence: 'moved' }, 'the first argument line, not the second');
});

// ---------------------------------------------------------------------------
// Corpus 2 — experiments/sync-testbed/values.txt through the three conflict drills. Six
// lines, edited one at a time, which is the smallest real "amend" sequence in the repo.
// ---------------------------------------------------------------------------

const VALUES_1 = trim(String.raw`
alpha
bravo
charlie-EXPORTED
delta-DOWNSTREAM
echo-IMPORTED
foxtrot-RENAMED-DOWNSTREAM
`);

const VALUES_3 = trim(String.raw`
alpha
bravo
charlie-EXPORTED
delta-EDITED-ON-GITHUB
echo-EDITED-ON-GITHUB
foxtrot-RENAMED-DOWNSTREAM
`);

check('an untouched neighbour survives edits around it', () => {
  const anchor = captureAnchor('values.txt', side, VALUES_1, 1);
  assertEqual(
    relocate(anchor, VALUES_3),
    { line: 1, confidence: 'exact' },
    'alpha did not move and its context above is still absent',
  );
});

check('two successive edits to the same line go stale', () => {
  const anchor = captureAnchor('values.txt', side, VALUES_1, 4);
  assertEqual(
    relocate(anchor, VALUES_3),
    { line: null, confidence: 'stale' },
    'delta-DOWNSTREAM is gone; there is nothing honest to point at',
  );
});

// ---------------------------------------------------------------------------
// Adversarial cases. These are the ones that decide whether the thing is safe to build on:
// a relocator that never says stale is a relocator that lies.
// ---------------------------------------------------------------------------

check('a repeated closing brace is placed by its block, not its text', () => {
  // `}` occurs twice; only the surrounding block tells them apart. Inserting a function
  // above shifts the anchored one from 3 to 6, and it must follow — this is the case a
  // naive first-match relocator gets wrong.
  const before = trim(String.raw`
fn a() {
  work();
}
fn b() {
  work();
}
`);
  const after = trim(String.raw`
fn zero() {
  nothing();
}
fn a() {
  work();
}
fn b() {
  work();
}
`);
  const anchor = captureAnchor('x.rs', side, before, 3);
  assertEqual(
    relocate(anchor, after),
    { line: 6, confidence: 'moved' },
    'the brace closing fn a(), which moved down by three lines',
  );
});

check('a genuinely ambiguous duplicate refuses to choose', () => {
  // The block is both MOVED (so the original position no longer holds it) and DUPLICATED
  // with identical surroundings, so the two candidates score identically. There is honestly
  // no way to know which copy the comment meant, and guessing is how a review tool puts a
  // comment on a line its author never read.
  const before = trim(String.raw`
pad();
fn a() {
  work();
}
pad();
more();
end();
`);
  const after = trim(String.raw`
header();
pad();
fn a() {
  work();
}
pad();
more();
end();
pad();
fn a() {
  work();
}
pad();
more();
end();
`);
  const anchor = captureAnchor('x.rs', side, before, 4);
  assertEqual(
    relocate(anchor, after),
    { line: null, confidence: 'stale' },
    'two identical placements must produce stale, not whichever was found first',
  );
});

check('a duplicate appearing elsewhere does not unseat an unmoved comment', () => {
  // Same shape, except the original position still holds the line. A copy appearing later
  // does not make the existing placement less true, so it must stay put.
  const before = trim(String.raw`
pad();
fn a() {
  work();
}
pad();
more();
end();
`);
  const after = trim(String.raw`
pad();
fn a() {
  work();
}
pad();
more();
end();
pad();
fn a() {
  work();
}
pad();
more();
end();
`);
  const anchor = captureAnchor('x.rs', side, before, 4);
  assertEqual(
    relocate(anchor, after),
    { line: 4, confidence: 'exact' },
    'the comment was already in the right place',
  );
});

check('a repeated line with distinguishing context is still followed', () => {
  const before = trim(String.raw`
setup();
value = 1;
teardown();
other();
value = 1;
finish();
`);
  const after = trim(String.raw`
extra();
setup();
value = 1;
teardown();
other();
value = 1;
finish();
`);
  const anchor = captureAnchor('x.rs', side, before, 2);
  assertEqual(
    relocate(anchor, after),
    { line: 3, confidence: 'moved' },
    'the occurrence between setup() and teardown(), not the one before finish()',
  );
});

check('a deleted file leaves every anchor stale', () => {
  const anchor = captureAnchor('x.rs', side, 'only();\n', 1);
  assertEqual(
    relocate(anchor, ''),
    { line: null, confidence: 'stale' },
    'an empty new version cannot host a comment',
  );
});

check('a blank line is never relocated on its own evidence', () => {
  const before = trim(String.raw`
a();

b();
`);
  const after = trim(String.raw`
a();

b();

c();
`);
  // Line 2 is blank; blank lines are everywhere. Context must carry it or it must go stale.
  const anchor = captureAnchor('x.rs', side, before, 2);
  const result = relocate(anchor, after);
  assertEqual(result, { line: 2, confidence: 'exact' }, 'context identifies the blank line');
});

if (failures > 0) throw new Error(`${failures} failing`);
console.log('all anchoring checks passed');

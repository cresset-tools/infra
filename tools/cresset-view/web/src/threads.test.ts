// Checks for placing threads onto a patch set.
//
// anchor.test.ts covers whether a *line* can be found. This covers what happens around that:
// the sides of a diff, a file that only exists on one side, corrupt stored context, and the
// ordering a reader actually scrolls through. Those are the cases that turn a correct
// relocation into a comment shown in the wrong place anyway.
//
// Dependency-free harness, matching graph.test.ts and anchor.test.ts. Run with `npm test`.
import { captureAnchor } from './anchor';
import { anchorOf, placeThreads, placementNote, unresolvedCount, type Thread } from './threads';

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

const trim = (text: string) => text.replace(/^\n/, '');

/// A file from this repository's own history: web/src/graph.ts around the lane assignment.
const BEFORE = trim(String.raw`
export function layoutRevisionGraph(revisions: Revision[], lanes: Array<string | null>) {
  const rows: GraphRow[] = [];
  for (const revision of revisions) {
    let lane = lanes.indexOf(revision.commit_id);
    if (lane === -1) lane = claimLane(lanes);
    rows.push({ lane, parents: [] });
  }
  return rows;
}
`);

const AFTER = trim(String.raw`
export function layoutRevisionGraph(revisions: Revision[], lanes: Array<string | null>) {
  const rows: GraphRow[] = [];
  // Carried between pages so a page continues the previous page's lane assignment.
  for (const revision of revisions) {
    let lane = lanes.indexOf(revision.commit_id);
    if (lane === -1) lane = claimLane(lanes);
    rows.push({ lane, parents: [] });
  }
  return rows;
}
`);

const PATH = 'web/src/graph.ts';

/// Build a thread the way the server would return one, anchoring it for real rather than
/// hand-writing a fingerprint — a hand-written one can agree with a broken capture.
function threadAt(id: number, line: number, side: 'additions' | 'deletions', contents: string): Thread {
  const anchor = captureAnchor(PATH, side, contents, line);
  return {
    id,
    change_id: 'kklyskvtvvstsmxyvnvptnrzknsqxnvw',
    path: PATH,
    side,
    line,
    fingerprint: anchor.fingerprint,
    context: JSON.stringify(anchor.context),
    resolved: false,
    created_by: 'jelle',
    created_at: 1_786_221_579,
    comments: [{
      id: id * 10,
      body: 'why is this claimed here?',
      author: 'jelle',
      patch_set_commit_id: 'a'.repeat(40),
      created_at: 1_786_221_579,
    }],
  };
}

check('a thread on an untouched line is placed exactly', () => {
  const thread = threadAt(1, 2, 'additions', BEFORE);
  const placed = placeThreads([thread], PATH, { before: BEFORE, after: BEFORE });
  assertEqual(placed.map((p) => [p.line, p.confidence]), [[2, 'exact']], 'same content, same line');
  assertEqual(placementNote(placed[0]), null, 'an exact placement needs no excuse');
});

check('a thread follows its line through an insertion above it', () => {
  // `if (lane === -1) lane = claimLane(lanes);` is line 5 before and line 6 after, pushed
  // down by the comment inserted at line 3.
  const thread = threadAt(1, 5, 'additions', BEFORE);
  const placed = placeThreads([thread], PATH, { before: BEFORE, after: AFTER });
  assertEqual(placed.map((p) => [p.line, p.confidence]), [[6, 'moved']], 'pushed down by one line');
  assertEqual(
    placementNote(placed[0]),
    'written against line 5 of an earlier patch set',
    'the reader must be told the line moved',
  );
});

check('a thread reads the side it was written against', () => {
  // The same line number holds different text on each side here, so a placement that ignored
  // `side` would relocate against the wrong content and land somewhere plausible-but-wrong.
  const deletionsOnly = trim(String.raw`
first();
second();
third();
`);
  const thread = threadAt(1, 2, 'deletions', deletionsOnly);
  const placed = placeThreads([thread], PATH, { before: deletionsOnly, after: BEFORE });
  assertEqual(placed.map((p) => [p.line, p.side, p.confidence]), [[2, 'deletions', 'exact']], 'read `before`');
});

check('a thread on a side the file does not have is file-level, not dropped', () => {
  // An added file has no `before`. A comment on its old text cannot be placed, and dropping
  // it silently would lose a remark someone is waiting on an answer to.
  const thread = threadAt(1, 2, 'deletions', BEFORE);
  const placed = placeThreads([thread], PATH, { before: null, after: AFTER });
  assertEqual(placed.map((p) => [p.line, p.confidence]), [[0, 'stale']], 'file-level');
});

check('threads on other paths are left alone', () => {
  const mine = threadAt(1, 2, 'additions', BEFORE);
  const theirs = { ...threadAt(2, 2, 'additions', BEFORE), path: 'web/src/main.ts' };
  const placed = placeThreads([mine, theirs], PATH, { before: BEFORE, after: BEFORE });
  assertEqual(placed.map((p) => p.thread.id), [1], 'only this file');
});

check('placements are ordered the way they are scrolled through', () => {
  const later = threadAt(1, 7, 'additions', BEFORE);
  const earlier = threadAt(2, 2, 'additions', BEFORE);
  const stale = { ...threadAt(3, 2, 'additions', BEFORE), fingerprint: 'gone forever' };
  const placed = placeThreads([later, earlier, stale], PATH, { before: BEFORE, after: BEFORE });
  assertEqual(
    placed.map((p) => [p.thread.id, p.line]),
    [[3, 0], [2, 2], [1, 7]],
    'file-level first, then by line',
  );
});

check('two threads on one line keep the order they were written in', () => {
  const second = threadAt(9, 2, 'additions', BEFORE);
  const first = threadAt(4, 2, 'additions', BEFORE);
  const placed = placeThreads([second, first], PATH, { before: BEFORE, after: BEFORE });
  assertEqual(placed.map((p) => p.thread.id), [4, 9], 'by id, which is insertion order');
});

check('a corrupt context costs precision, not the whole file', () => {
  const thread = { ...threadAt(1, 5, 'additions', BEFORE), context: 'not json at all' };
  assertEqual(anchorOf(thread).context, [], 'unparseable context reads as none');
  const placed = placeThreads([thread], PATH, { before: BEFORE, after: AFTER });
  // The fingerprint is unique in the file, so it still relocates — context only breaks ties.
  assertEqual(placed.map((p) => [p.line, p.confidence]), [[6, 'moved']], 'still placed');
});

check('a context of the wrong shape is refused as firmly as bad json', () => {
  const thread = { ...threadAt(1, 5, 'additions', BEFORE), context: '[1, 2, 3]' };
  assertEqual(anchorOf(thread).context, [], 'an array of numbers is not context');
});

check('a resolved thread is placed but not counted', () => {
  const open = threadAt(1, 2, 'additions', BEFORE);
  const done = { ...threadAt(2, 5, 'additions', BEFORE), resolved: true };
  assertEqual(unresolvedCount([open, done]), 1, 'one thread still wants an answer');
  assertEqual(placeThreads([open, done], PATH, { before: BEFORE, after: BEFORE }).length, 2, 'both shown');
});

check('a line edited away since the comment was written says so', () => {
  const thread = threadAt(1, 2, 'additions', BEFORE);
  const rewritten = BEFORE.replace('  const rows: GraphRow[] = [];', '  const rows: Row[] = [];');
  if (rewritten === BEFORE) throw new Error('fixture: the rewrite did not apply');
  const placed = placeThreads([thread], PATH, { before: BEFORE, after: rewritten });
  assertEqual(placed.map((p) => [p.line, p.confidence]), [[0, 'stale']], 'nothing honest to point at');
  assertEqual(
    placementNote(placed[0]),
    'written against line 2, which has since changed',
    'and the card must say why it is at the top of the file',
  );
});

if (failures > 0) throw new Error(`${failures} failing`);
console.log('all thread placement checks passed');

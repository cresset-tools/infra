// Checks for the revision topology layout, kept apart from the viewer so they run without a DOM.
//
// Deliberately dependency-free: no `node:test` (this Node build's runner fails to start with
// `Missing internal module 'internal/deps/brace-expansion'`) and no `node:assert` (which would
// drag @types/node into the app's typecheck). A checked-in test that cannot run is worse than
// no test, and a test that breaks `npm run build` is worse still. Run with `npm test`.
import { layoutRevisionGraph, type GraphCommit } from './graph';

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

function assertSame(actual: unknown, expected: unknown, message: string) {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new Error(`${message}\n     actual:   ${a}\n     expected: ${b}`);
}

function assertTrue(value: boolean, message: string) {
  if (!value) throw new Error(message);
}

/// A history with a side branch and a merge, newest first — the shape the viewer renders.
function history(): GraphCommit[] {
  return [
    { commit_id: 'k', parent_commit_ids: ['j'] },
    { commit_id: 'j', parent_commit_ids: ['i', 'f'] }, // merge
    { commit_id: 'i', parent_commit_ids: ['h'] },
    { commit_id: 'h', parent_commit_ids: ['e'] },
    { commit_id: 'f', parent_commit_ids: ['e'] }, // side branch
    { commit_id: 'e', parent_commit_ids: ['d'] },
    { commit_id: 'd', parent_commit_ids: ['c'] },
    { commit_id: 'c', parent_commit_ids: ['b'] },
    { commit_id: 'b', parent_commit_ids: ['a'] },
    { commit_id: 'a', parent_commit_ids: [] },
  ];
}

// Paging must not change the picture.
//
// The layout carries lane state from one page into the next, so a page boundary is the one place
// this can go wrong — and it fails silently, by drawing a history that is subtly not the one in
// the repository. Splitting at every possible boundary is cheap and covers the boundary landing
// mid-merge, which is the interesting case.
check('paging produces the same topology as a single pass', () => {
  const all = history();
  const whole = layoutRevisionGraph(all, []);
  for (let split = 1; split < all.length; split += 1) {
    const lanes: Array<string | null> = [];
    const paged = [
      ...layoutRevisionGraph(all.slice(0, split), lanes),
      ...layoutRevisionGraph(all.slice(split), lanes),
    ];
    assertSame(paged, whole, `splitting after ${split} row(s) changed the topology`);
  }
});

// Three pages, to catch state that survives one boundary but not two.
check('the topology survives repeated page boundaries', () => {
  const all = history();
  const whole = layoutRevisionGraph(all, []);
  const lanes: Array<string | null> = [];
  const paged = [
    ...layoutRevisionGraph(all.slice(0, 3), lanes),
    ...layoutRevisionGraph(all.slice(3, 7), lanes),
    ...layoutRevisionGraph(all.slice(7), lanes),
  ];
  assertSame(paged, whole, 'three pages disagreed with one pass');
});

// Without this the equality above could hold trivially over a single-lane graph.
check('the fixture really does branch', () => {
  const rows = layoutRevisionGraph(history(), []);
  assertTrue(
    rows.some((row) => row.laneCount > 1),
    'the fixture must produce a multi-lane graph, or paging equality proves nothing',
  );
});

if (failures > 0) throw new Error(`${failures} failing`);
console.log('all graph layout checks passed');

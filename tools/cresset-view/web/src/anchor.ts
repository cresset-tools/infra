// Relocating a review comment onto the right line of a later patch set.
//
// This is the crux of review-in-cresset-view, and the reason it is built before anything
// else. jj change ids make a *thread* follow a change across amends for free; nothing makes a
// *comment* follow a line, because the content moves underneath it. A review tool whose
// comments drift onto the wrong lines is worse than no review tool — the reader trusts a
// placement that is a lie.
//
// The design rule throughout: **when in doubt, say stale**. A comment shown at file level with
// "this line has changed" is honest and mildly annoying. A comment shown confidently on the
// wrong line is a defect that reads as a human error by whoever wrote it.
//
// DOM-free and dependency-free, following web/src/graph.ts, so it can be exercised by
// `npm test` in the Nix sandbox without a browser.

/// Which side of a diff a comment is attached to, matching @pierre/diffs' `AnnotationSide`.
export type Side = 'deletions' | 'additions';

export type Confidence =
  /// The line is exactly where it was, with its surroundings intact.
  | 'exact'
  /// Found elsewhere, with enough agreeing context to be sure it is the same line.
  | 'moved'
  /// Not found, or found in more than one equally plausible place. Show at file level.
  | 'stale';

/// What is persisted with a comment so it can be placed in a later patch set.
///
/// `line` is the 1-based line number in the patch set the comment was written against, and is
/// only a hint on relocation — the fingerprint and context are what actually identify the line.
export interface Anchor {
  path: string;
  side: Side;
  line: number;
  fingerprint: string;
  context: string[];
}

export interface Relocation {
  /// 1-based line in the new patch set, or `null` when stale.
  line: number | null;
  confidence: Confidence;
}

/// Lines of context kept on each side of the anchored line.
///
/// Three is enough to disambiguate repeated lines like `}` or `});` in practice, and small
/// enough that an edit a few lines away does not destroy the anchor by itself — context is
/// scored, not required to match wholesale.
const CONTEXT = 3;

/// Split into lines and strip trailing whitespace.
///
/// Trailing whitespace only: leading indentation is meaningful. A line that moved into a
/// different block genuinely is a different line, and treating `  foo()` as equal to
/// `      foo()` would relocate comments across scopes.
function linesOf(contents: string): string[] {
  return contents.replace(/\n$/, '').split('\n').map((line) => line.replace(/\s+$/, ''));
}

/// Capture an anchor for `line` (1-based) in `contents`.
export function captureAnchor(path: string, side: Side, contents: string, line: number): Anchor {
  const lines = linesOf(contents);
  const index = line - 1;
  const context: string[] = [];
  for (let offset = -CONTEXT; offset <= CONTEXT; offset += 1) {
    if (offset === 0) continue;
    // Out-of-range context is recorded as the empty string rather than skipped, so that
    // "third line of the file" keeps its distinguishing shape: its leading context is
    // genuinely absent, and a candidate in the middle of the file should not score for it.
    context.push(lines[index + offset] ?? '');
  }
  return { path, side, line, fingerprint: lines[index] ?? '', context };
}

/// How many context lines around `index` agree with the anchor's, positionally.
function contextScore(lines: string[], index: number, context: string[]): number {
  let score = 0;
  let slot = 0;
  for (let offset = -CONTEXT; offset <= CONTEXT; offset += 1) {
    if (offset === 0) continue;
    const expected = context[slot];
    slot += 1;
    if (expected === undefined) continue;
    if ((lines[index + offset] ?? '') === expected) score += 1;
  }
  return score;
}

/// Place `anchor` in a later patch set's `contents`.
export function relocate(anchor: Anchor, contents: string): Relocation {
  const lines = linesOf(contents);
  const original = anchor.line - 1;

  // Unchanged in place, surroundings and all. The common case, and worth answering without
  // searching so an untouched file cannot be reported as `moved` by a coincidence elsewhere.
  if (
    lines[original] === anchor.fingerprint &&
    contextScore(lines, original, anchor.context) === anchor.context.length
  ) {
    return { line: anchor.line, confidence: 'exact' };
  }

  const candidates: number[] = [];
  for (let index = 0; index < lines.length; index += 1) {
    if (lines[index] === anchor.fingerprint) candidates.push(index);
  }
  // The line is gone. Deleted, or edited — either way there is nothing honest to point at.
  if (candidates.length === 0) return { line: null, confidence: 'stale' };

  // A line that occurs once is identified by its own text; context only has to break ties.
  if (candidates.length === 1) {
    const index = candidates[0];
    return {
      line: index + 1,
      confidence: index === original ? 'exact' : 'moved',
    };
  }

  const scored = candidates.map((index) => ({
    index,
    score: contextScore(lines, index, anchor.context),
    // Distance is the tie-breaker of last resort and is deliberately NOT used to pick a
    // winner — only to prefer the original position when context is genuinely equal, which
    // is the "nothing moved, a duplicate appeared elsewhere" case.
    distance: Math.abs(index - original),
  }));
  scored.sort((a, b) => b.score - a.score || a.distance - b.distance);

  const best = scored[0];
  const runnerUp = scored[1];

  // Ambiguous: two places fit equally well. Refusing here is the whole point — this is the
  // case that produces a confidently wrong placement in tools that guess.
  //
  // The exception is a duplicate at the original position: if the best candidate IS where the
  // comment already was, a tie elsewhere does not make the placement less true.
  if (runnerUp.score === best.score && best.index !== original) {
    return { line: null, confidence: 'stale' };
  }

  // Context contributed nothing — every candidate scored zero. The fingerprint alone is not
  // evidence when the line occurs many times (`}`, `});`, a blank line).
  if (best.score === 0) return { line: null, confidence: 'stale' };

  return {
    line: best.index + 1,
    confidence: best.index === original ? 'exact' : 'moved',
  };
}

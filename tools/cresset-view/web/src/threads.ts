// Placing review threads onto the patch set being read.
//
// The server stores a thread's anchor and never interprets it (see src/review.rs). This is
// where it gets interpreted: given the threads on a change and the two sides of a file's diff,
// work out which line each thread belongs on *in this version*, and how much to trust that.
//
// Split out of main.ts and kept DOM-free so it can be exercised by `npm test`, following
// graph.ts and anchor.ts. The rendering lives in main.ts; the deciding lives here, because
// deciding wrong is the failure that matters.

import { relocate, type Anchor, type Confidence, type Side } from './anchor';

export interface Comment {
  id: number;
  body: string;
  author: string;
  patch_set_commit_id: string;
  created_at: number;
}

/// A thread as the server sends it. `context` is a JSON array of strings, kept as text end to
/// end so the store never has to know what an anchor is.
export interface Thread {
  id: number;
  change_id: string;
  path: string;
  side: Side;
  line: number;
  fingerprint: string;
  context: string;
  resolved: boolean;
  created_by: string;
  created_at: number;
  comments: Comment[];
}

export interface PlacedThread {
  thread: Thread;
  /// 1-based line in the patch set being read, or 0 for a file-level placement — which is what
  /// @pierre/diffs' `lineNumber: 0` means, and where a stale thread goes.
  line: number;
  side: Side;
  confidence: Confidence;
}

/// The sides of a file as the diff stream delivers them. `null` means the file does not exist
/// on that side: added files have no `before`, deleted files have no `after`.
export interface FileSides {
  before: string | null;
  after: string | null;
}

/// Rebuild an `Anchor` from a stored thread.
///
/// A row whose `context` is not a JSON array of strings is treated as having no context rather
/// than throwing. Bad data should cost precision — the thread relocates on its fingerprint
/// alone, and a fingerprint with no context that occurs more than once goes stale — not the
/// whole file's worth of comments.
export function anchorOf(thread: Thread): Anchor {
  let context: string[] = [];
  try {
    const parsed: unknown = JSON.parse(thread.context);
    if (Array.isArray(parsed) && parsed.every((line) => typeof line === 'string')) {
      context = parsed as string[];
    }
  } catch {
    context = [];
  }
  return {
    path: thread.path,
    side: thread.side,
    line: thread.line,
    fingerprint: thread.fingerprint,
    context,
  };
}

/// Place every thread on `path` into the version of the file being read.
///
/// Threads whose side is missing from this patch set — a comment on the old text of a file
/// that is now added, say — are file-level rather than dropped. A comment that disappears
/// because the code moved is a comment nobody answers.
export function placeThreads(threads: Thread[], path: string, sides: FileSides): PlacedThread[] {
  const placed: PlacedThread[] = [];
  for (const thread of threads) {
    if (thread.path !== path) continue;
    const contents = thread.side === 'additions' ? sides.after : sides.before;
    if (contents == null) {
      placed.push({ thread, line: 0, side: thread.side, confidence: 'stale' });
      continue;
    }
    const { line, confidence } = relocate(anchorOf(thread), contents);
    placed.push({ thread, line: line ?? 0, side: thread.side, confidence });
  }
  // Ordered so the annotations a reader meets while scrolling are in the order they appear,
  // and two threads on one line keep the order they were written in.
  placed.sort((a, b) => a.line - b.line || a.thread.id - b.thread.id);
  return placed;
}

/// How many threads still want an answer. Resolved ones stay visible but do not count.
export function unresolvedCount(threads: Thread[]): number {
  return threads.filter((thread) => !thread.resolved).length;
}

/// One line of explanation for a placement, or `null` when the placement speaks for itself.
///
/// Only ever *reduces* confidence in what is shown. A reader who is not told that a comment
/// moved will read it against the wrong line and conclude the author was careless.
export function placementNote(placed: PlacedThread): string | null {
  if (placed.confidence === 'exact') return null;
  if (placed.confidence === 'moved') {
    return `written against line ${placed.thread.line} of an earlier patch set`;
  }
  return `written against line ${placed.thread.line}, which has since changed`;
}

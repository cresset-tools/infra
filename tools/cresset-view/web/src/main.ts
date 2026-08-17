import {
  CodeView,
  File as FileViewer,
  parseDiffFromFile,
  type CodeViewItem,
  type CodeViewOptions,
  type DiffLineAnnotation,
  type FileDiffMetadata,
  type ThemeTypes,
} from '@pierre/diffs';
import { FileTree, prepareFileTreeInput, type FileTreePreparedInput } from '@pierre/trees';
// The bougie brand faces, self-hosted so the viewer looks the same on every machine.
import '@fontsource/archivo/400.css';
import '@fontsource/archivo/500.css';
import '@fontsource/archivo/600.css';
import '@fontsource/archivo/800.css';
import '@fontsource/archivo/900.css';
import '@fontsource/jetbrains-mono/400.css';
import '@fontsource/jetbrains-mono/500.css';
import '@fontsource/jetbrains-mono/700.css';
import './style.css';
import { graphLaneX, layoutRevisionGraph, type GraphRow } from './graph';
import { captureAnchor, type Confidence, type Side } from './anchor';
import {
  placeThreads,
  placementNote,
  unresolvedCount,
  type FileSides,
  type PlacedThread,
  type Thread,
} from './threads';

interface Revision {
  change_id: string;
  commit_id: string;
  parent_commit_ids: string[];
  description: string;
  author_name: string;
  author_email: string;
  authored_at: string;
  has_conflict: boolean;
  divergent: boolean;
  working_copy: boolean;
  is_head: boolean;
  bookmarks: string[];
}

interface RevisionsResponse {
  operation_id: string;
  head_count: number;
  revisions: Revision[];
  has_more: boolean;
}

interface FileChange {
  index: number;
  path: string;
  before: string | null;
  after: string | null;
  conflicted: boolean;
  binary: boolean;
}

interface DiffMetadata {
  operation_id: string;
  change_id: string;
  commit_id: string;
  paths: string[];
}

type DiffEvent = ({ type: 'metadata' } & DiffMetadata)
  | ({ type: 'file' } & FileChange)
  | { type: 'error'; error: string };

interface TreeResponse {
  operation_id: string;
  change_id: string;
  commit_id: string;
  paths: Array<{
    path: string;
    kind: string;
    conflicted: boolean;
  }>;
}

interface FileResponse {
  operation_id: string;
  change_id: string;
  commit_id: string;
  path: string;
  contents: string | null;
  conflicted: boolean;
  binary: boolean;
  conflict?: ConflictView;
}

interface ChangeSummary {
  change_id: string;
  commit_id: string;
  description: string;
  author_name: string;
  authored_at: string;
  patch_sets: number;
  has_conflict: boolean;
}

/// One review bookmark and the changes it carries — Gerrit's relation chain.
///
/// Landing is a property of the stack, not of a change: advancing main to the tip lands
/// everything beneath it. So the queue groups by bookmark and the Merge button belongs here.
interface Stack {
  bookmark: string;
  tip: string;
  /// Oldest first: the order they would land in.
  changes: ChangeSummary[];
}

interface ChangesResponse {
  operation_id: string;
  stacks: Stack[];
}

interface PatchSet {
  number: number;
  commit_id: string;
  current: boolean;
}

interface Approval {
  change_id: string;
  commit_id: string;
  approved_by: string;
  created_at: number;
}

interface ApprovalsResponse {
  change_id: string;
  approvals: Approval[];
  /// Whether this instance actually gates pushes. False means approving records who read what
  /// and nothing enforces it — worth saying rather than implying a gate that is not there.
  gated: boolean;
}

interface ChangeDetail {
  operation_id: string;
  change_id: string;
  current: Revision;
  bookmark?: string;
  patch_sets: PatchSet[];
}

interface ConflictTerm {
  label?: string;
  contents?: string;
  absent: boolean;
  binary: boolean;
}

interface ConflictView {
  bases: ConflictTerm[];
  sides: ConflictTerm[];
  materialized?: string;
}

type ViewMode = 'browse' | 'changes' | 'review';
type ThemePreference = 'auto' | 'light' | 'dark';

/// What a `CodeView` annotation carries: an anchor, and the thread behind it once there is one.
///
/// A draft is an annotation too — the composer opens where the comment will land, so what you
/// type is already in the place you are talking about — which is why `placed` is optional rather
/// than this being a union of two shapes. @pierre/diffs' `OptionalMetadata<T>` distributes over
/// a union, so a union here makes `metadata` unassignable at the call site.
interface ThreadAnnotation {
  path: string;
  side: Side;
  line: number;
  /// Absent while the comment is still being written.
  placed?: PlacedThread;
}

const app = document.querySelector<HTMLElement>('#app');
if (app == null) throw new Error('missing app element');

app.innerHTML = `
  <header>
    <div class="wordmark">
      <span class="eyebrow">Cresset internal</span>
      <h1>Code</h1>
    </div>
    <div class="header-actions">
      <div class="mode-switch" aria-label="Viewer mode">
        <button type="button" data-mode="browse">Browse</button>
        <button type="button" data-mode="changes">Changes</button>
        <button type="button" data-mode="review">Review</button>
      </div>
      <label class="theme-control">
        <span>Theme</span>
        <select id="theme">
          <option value="auto">Auto</option>
          <option value="light">Light</option>
          <option value="dark">Dark</option>
        </select>
      </label>
      <code id="operation">loading operation</code>
    </div>
  </header>
  <div id="sync-banner" class="sync-banner" hidden></div>
  <section class="workspace">
    <aside class="changes">
      <h2><span id="panel-title">Revisions</span><strong id="head-count"></strong></h2>
      <label class="revision-search">
        <input id="revision-search" type="search" placeholder="Search description, author, or id"
               aria-label="Search revisions" autocomplete="off" spellcheck="false">
      </label>
      <label class="revision-pan">
        <span>Pan topology</span>
        <input id="revision-pan" type="range" min="0" value="0" aria-label="Pan revision topology">
      </label>
      <div id="revision-list" class="revision-list"></div>
    </aside>
    <aside class="files">
      <h2 id="file-heading">Files</h2>
      <div id="file-tree"></div>
    </aside>
    <article class="detail">
      <div id="change-heading" class="change-heading">
        <p>Loading the current revision.</p>
      </div>
      <div id="content"></div>
    </article>
  </section>
`;

const syncBanner = requiredElement('#sync-banner');
const operation = requiredElement('#operation');
const revisionList = requiredElement('#revision-list');
const headCount = requiredElement('#head-count');
const panelTitle = requiredElement('#panel-title');
const revisionPan = requiredElement<HTMLInputElement>('#revision-pan');
const revisionSearch = requiredElement<HTMLInputElement>('#revision-search');
const fileTreeContainer = requiredElement('#file-tree');
const fileHeading = requiredElement('#file-heading');
const changeHeading = requiredElement('#change-heading');
const content = requiredElement('#content');
const themeSelect = requiredElement<HTMLSelectElement>('#theme');
const modeButtons = [...document.querySelectorAll<HTMLButtonElement>('[data-mode]')];
let tree: FileTree | null = null;
let renderedFile: FileViewer | null = null;
let codeView: CodeView<ThreadAnnotation> | null = null;
let diffItems: CodeViewItem<ThreadAnnotation>[] = [];
let loadedDiffPaths = new Set<string>();
let currentRevision: Revision | null = null;
let currentRevisionButton: HTMLButtonElement | null = null;
// The revision list is paged, so it is state rather than one response. `graphLanes` is carried
// between pages: the graph layout is a single top-down pass, so a page can continue the previous
// page's lane assignment instead of re-laying-out everything already on screen.
const loadedRevisions: Revision[] = [];
const revisionButtons = new Map<string, HTMLButtonElement>();
let graphLanes: Array<string | null> = [];
let graphMaxLanes = 1;
let graphWidth = -1;
let revisionOffset = 0;
let revisionHasMore = false;
let revisionQuery = '';
let revisionLoading = false;
// Bumped on every reset so an in-flight page from a superseded search cannot append to the list
// that replaced it.
let revisionGeneration = 0;
let revisionMoreButton: HTMLButtonElement | null = null;
let revisionStatus: HTMLParagraphElement | null = null;
let revisionObserver: IntersectionObserver | null = null;
let searchDebounce: number | undefined;
const initialUrlState = readUrlState();
let currentMode: ViewMode = initialUrlState.path == null ? 'browse' : 'changes';
let selectionGeneration = 0;
let fileGeneration = 0;
let diffController: AbortController | null = null;
let diffParser: DiffParser | null = null;
let themePreference = readThemePreference();

// --- Review threads -------------------------------------------------------------------------
// Only populated while a change is open. Browse and Changes read arbitrary revisions, which have
// no change under review and so no threads and no gutter affordance.
let reviewChangeId: string | null = null;
let reviewPatchSet: string | null = null;
let reviewThreads: Thread[] = [];
let reviewApprovals: Approval[] = [];
let reviewGated = false;
/// Who this browser is, as the proxy sees it. Fetched once; the header never reaches the page,
/// so the UI cannot tell "your" approval from anyone else's without asking.
let reviewIdentity = '';
/// Every approval on the instance, so the queue can mark each change without a request each.
let reviewAllApprovals: Approval[] = [];
/// What was landed in this session, and is therefore expected to still be listed until the
/// repository snapshot catches up. Cleared once it is genuinely gone.
let justMerged: { bookmark: string; tip: string } | null = null;
/// Whether this instance can land a stack at all. False hides Merge rather than offering a
/// button that answers "merging is not available on this instance".
let reviewCanMerge = false;
/// Whether this instance can be written to. `--review-db` is optional, so a viewer can run with
/// review read-only; the "+" must not be offered if a comment would be refused.
let reviewWritable = false;
/// Both sides of every file loaded from the diff stream, kept because an anchor is captured and
/// relocated against file CONTENT, and the parsed `FileDiffMetadata` has already thrown the
/// original text away.
const fileSides = new Map<string, FileSides>();
/// `CodeView.updateItem` is a no-op unless `version` changes, so every rebuild of an item has to
/// carry a higher number than the last one for that path.
const itemVersions = new Map<string, number>();
/// At most one composer is open. A second "+" click moves it rather than opening a second box —
/// two half-written comments in one file is a way to lose one of them.
/// Carries its own annotation object so its identity is stable while it is open — see
/// `cachedAnnotation` for why that matters.
let draft: { path: string; side: Side; line: number; annotation: ThreadAnnotation } | null = null;
/// What has been typed but not sent, by draft key and by thread id. Virtualization unmounts a
/// file that scrolls out of view and takes its DOM with it, so the text has to live out here.
let draftBody = '';
const replyBodies = new Map<number, string>();
/// Annotation metadata is compared by identity to decide whether to re-render a card (see
/// `areDiffLineAnnotationsEqual`), so the same placement must hand back the same object or every
/// scroll rebuilds the DOM and drops focus mid-sentence.
const annotationCache = new Map<string, ThreadAnnotation>();

themeSelect.value = themePreference;
applyTheme(themePreference);
themeSelect.addEventListener('change', () => {
  themePreference = themeSelect.value as ThemePreference;
  localStorage.setItem('cresset-view-theme', themePreference);
  applyTheme(themePreference);
});

for (const button of modeButtons) {
  button.addEventListener('click', () => void setMode(button.dataset.mode as ViewMode));
}
syncModeButtons();

void initialize();
// Independent of the main load: the banner must not delay the viewer, and a worker that is
// absent or unreachable must not stop the repository rendering.
void renderSyncStatus();

async function initialize() {
  revisionSearch.addEventListener('input', () => {
    // Debounced: each keystroke would otherwise walk the whole revset server-side.
    window.clearTimeout(searchDebounce);
    searchDebounce = window.setTimeout(() => void applySearch(revisionSearch.value), 200);
  });
  revisionSearch.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && revisionSearch.value !== '') {
      event.preventDefault();
      revisionSearch.value = '';
      void applySearch('');
    }
  });

  revisionPan.addEventListener('input', applyGraphPan);

  // Not fatal if it fails: the viewer is useful without knowing who is looking, and an empty
  // identity simply means no approval is shown as yours.
  const me = await fetchJson<{ username: string; can_merge: boolean }>('/api/identity')
    .catch(() => ({ username: '', can_merge: false }));
  reviewIdentity = me.username;
  reviewCanMerge = me.can_merge;

  const first = await loadRevisionPage(true);
  if (first == null) return;
  operation.textContent = `operation ${short(first.operation_id)}`;
  headCount.textContent = `${first.head_count.toLocaleString()} heads`;

  // A `?change=` link goes straight to the review queue with that change open. It is the
  // destination hooks/update prints when it refuses a push, so it has to work from cold.
  if (initialUrlState.change != null) {
    currentMode = 'review';
    syncModeButtons();
    await loadReviewQueue(initialUrlState.change);
    window.addEventListener('popstate', () => void restoreUrlState());
    return;
  }

  let initialRevision = initialUrlState.revision == null
    ? loadedRevisions.find((revision) => revision.working_copy) ?? loadedRevisions[0]
    : findRevision(loadedRevisions, initialUrlState.revision);
  // A revision outside the first page is normal now that the list is paged — 3,700 revisions
  // are reachable and 100 are loaded — so fall back to fetching it directly rather than
  // showing nothing for a link someone shared.
  if (initialRevision == null && initialUrlState.revision != null) {
    initialRevision = (await fetchRevisionOrNull(initialUrlState.revision)) ?? undefined;
    if (initialRevision == null) renderMissingRevision(initialUrlState.revision);
  }
  if (initialRevision != null) {
    // A path in the URL pins changes mode (the link points at a diff); otherwise the revision
    // decides, same as a click would.
    if (initialUrlState.path == null) {
      currentMode = defaultModeFor(initialRevision);
      syncModeButtons();
    }
    await selectRevision(initialRevision, revisionButtons.get(initialRevision.commit_id) ?? null, false);
  }
  window.addEventListener('popstate', () => void restoreUrlState());
}

/// Reset to an empty list and load the first page for `query`.
async function applySearch(raw: string) {
  const next = raw.trim();
  if (next === revisionQuery) return;
  revisionQuery = next;
  await loadRevisionPage(true);
}

/// Fetch one page and append it. `reset` clears the list first (a new search, or the first load).
async function loadRevisionPage(reset: boolean): Promise<RevisionsResponse | null> {
  if (revisionLoading && !reset) return null;
  revisionLoading = true;
  if (reset) {
    revisionGeneration += 1;
    loadedRevisions.length = 0;
    revisionButtons.clear();
    graphLanes = [];
    graphMaxLanes = 1;
    revisionOffset = 0;
    revisionList.replaceChildren();
    // Some engines keep the old scroll offset past the end of the now-shorter list, which
    // shows a blank pane until the reader scrolls back up.
    revisionList.scrollTop = 0;
    revisionPan.value = '0';
    applyGraphPan();
    revisionMoreButton = null;
    revisionStatus = null;
    revisionObserver?.disconnect();
    revisionObserver = null;
  }
  const generation = revisionGeneration;
  setMoreState('loading');

  const url = new URL('/api/revisions', location.origin);
  url.searchParams.set('limit', '100');
  url.searchParams.set('offset', String(revisionOffset));
  if (revisionQuery !== '') url.searchParams.set('q', revisionQuery);

  let response: RevisionsResponse;
  try {
    response = await fetchJson<RevisionsResponse>(url.pathname + url.search);
  } catch (error) {
    revisionLoading = false;
    if (generation === revisionGeneration) setMoreState('error');
    throw error;
  }
  // A superseded search finished after the one that replaced it; drop it.
  if (generation !== revisionGeneration) {
    revisionLoading = false;
    return null;
  }

  appendRevisionRows(response.revisions);
  revisionOffset += response.revisions.length;
  revisionHasMore = response.has_more;
  revisionLoading = false;
  setMoreState(revisionHasMore ? 'more' : 'done');
  syncRevisionPanRange();
  return response;
}

/// Record the full drawn width of the topology, keeping every rendered row in agreement.
///
/// The graph COLUMN is a fixed width (`--graph-col`, CSS): a later page introducing a new lane
/// used to widen the shared column, which visibly re-laid-out every row already on screen the
/// moment a page arrived. Instead the drawing keeps its full width (`--graph-full-width`) inside
/// the fixed window and the pan slider translates it. Each row's SVG bakes this width into its
/// `viewBox`, and `preserveAspectRatio="none"` would stretch a stale one sideways off the nodes,
/// so when the width grows the already-rendered viewBoxes are rewritten to match.
function setGraphWidth(width: number) {
  if (width === graphWidth) return;
  graphWidth = width;
  revisionList.style.setProperty('--graph-full-width', `${width}px`);
  for (const svg of revisionList.querySelectorAll('.revision-graph svg')) {
    svg.setAttribute('viewBox', `0 0 ${width} 100`);
  }
  syncRevisionPanRange();
}

function appendRevisionRows(revisions: Revision[]) {
  // In search mode the visible rows are not contiguous history, so a topology graph drawn
  // through them would connect commits that are not adjacent — a picture that is simply false.
  // Show the rows without it instead.
  const searching = revisionQuery !== '';
  const rows = searching ? null : layoutRevisionGraph(revisions, graphLanes);
  if (rows != null) {
    graphMaxLanes = Math.max(graphMaxLanes, ...rows.map((row) => row.laneCount));
  }
  const laneGap = 14;
  const width = searching ? 0 : Math.ceil(24 + (graphMaxLanes - 1) * laneGap);
  setGraphWidth(width);
  // Rows have no graph child while searching, so they need a single-column grid. Carried on the
  // container so it applies to pages appended later too.
  revisionList.classList.toggle('searching', searching);
  // No graph means nothing to pan; leaving the slider visible offers a control that does
  // nothing, which is worse than not offering it.
  revisionPan.closest('.revision-pan')?.toggleAttribute('hidden', searching);

  const fragment = document.createDocumentFragment();
  for (const [index, revision] of revisions.entries()) {
    const button = document.createElement('button');
    button.className = 'revision';
    button.innerHTML = `
      ${rows == null ? '' : renderRevisionGraph(rows[index], revision, width, laneGap)}
      <span class="revision-copy">
        <span class="revision-id">${revision.working_copy ? '@ · ' : ''}${escapeHtml(short(revision.change_id))}</span>
        <strong>${escapeHtml(firstLine(revision.description) || '(no description)')}</strong>
        <small>${escapeHtml(revision.author_name)} · ${formatDateTime(revision.authored_at)}</small>
        <span class="signals">
          ${revision.bookmarks.map((name) => `<em>${escapeHtml(name)}</em>`).join('')}
          ${revision.working_copy ? '<em>@</em>' : ''}
          ${revision.is_head ? '<em class="head">head</em>' : ''}
          ${revision.divergent ? '<em class="warning">divergent</em>' : ''}
          ${revision.has_conflict ? '<em class="warning">conflict</em>' : ''}
        </span>
      </span>
    `;
    button.addEventListener('click', () => void selectRevision(revision, button, true));
    revisionButtons.set(revision.commit_id, button);
    loadedRevisions.push(revision);
    fragment.append(button);
  }
  // Insert before the footer so the "load more" control stays at the bottom.
  revisionList.insertBefore(fragment, revisionMoreButton ?? revisionStatus ?? null);
  if (currentRevision != null) {
    const button = revisionButtons.get(currentRevision.commit_id);
    if (button != null) {
      button.classList.add('selected');
      currentRevisionButton = button;
    }
  }
}

/// The footer of the list: a load-more button, a terminal message, or nothing.
function setMoreState(state: 'loading' | 'more' | 'done' | 'error') {
  revisionStatus?.remove();
  revisionStatus = null;
  if (state === 'more') {
    if (revisionMoreButton == null) {
      revisionMoreButton = document.createElement('button');
      revisionMoreButton.type = 'button';
      revisionMoreButton.className = 'revision-more';
      revisionMoreButton.addEventListener('click', () => void loadRevisionPage(false));
      revisionList.append(revisionMoreButton);
      // Scrolling the button into view loads the next page, so the button is a fallback for
      // anyone who reaches it by keyboard rather than the only way through.
      revisionObserver = new IntersectionObserver((entries) => {
        if (entries.some((entry) => entry.isIntersecting) && !revisionLoading && revisionHasMore) {
          void loadRevisionPage(false);
        }
      }, { root: revisionList, rootMargin: '400px' });
      revisionObserver.observe(revisionMoreButton);
    }
    revisionMoreButton.disabled = false;
    revisionMoreButton.textContent = 'Load more';
    // Re-observe after every page: the button is moved down rather than re-created, so if it
    // is still inside the root margin after a page appends, its intersection state never
    // changed and the observer would stay silent — the list stalled until the next scroll.
    // Observing anew always reports the current state.
    revisionObserver?.unobserve(revisionMoreButton);
    revisionObserver?.observe(revisionMoreButton);
    return;
  }
  if (state === 'loading') {
    if (revisionMoreButton != null) {
      revisionMoreButton.disabled = true;
      revisionMoreButton.textContent = 'Loading…';
    }
    return;
  }
  revisionObserver?.disconnect();
  revisionObserver = null;
  revisionMoreButton?.remove();
  revisionMoreButton = null;
  revisionStatus = document.createElement('p');
  revisionStatus.className = 'revision-status';
  if (state === 'error') {
    revisionStatus.textContent = 'Could not load revisions.';
  } else if (loadedRevisions.length === 0) {
    revisionStatus.textContent = `No revision matches ${revisionQuery}.`;
  } else if (revisionQuery !== '') {
    const n = loadedRevisions.length;
    revisionStatus.textContent = `${n} matching revision${n === 1 ? '' : 's'}.`;
  } else {
    revisionStatus.textContent = 'End of history.';
  }
  revisionList.append(revisionStatus);
}

/// The mode a revision opens in before anyone touches the switch.
///
/// The working copy is where the tree as it stands is the interesting object — you browse it.
/// Any other revision is usually visited to see what it changed, so it opens on its diff. The
/// header switch still overrides either, for the revision currently selected; picking another
/// revision returns to its default.
function defaultModeFor(revision: Revision): ViewMode {
  return revision.working_copy ? 'browse' : 'changes';
}

/// Load the review queue into the revisions panel.
///
/// The queue reuses that panel rather than adding a fourth column: a change IS a revision
/// here, so selecting one behaves like selecting a revision and the files and detail panes
/// carry on working unchanged.
async function loadReviewQueue(wanted: string | null = null) {
  panelTitle.textContent = 'Review';
  revisionSearch.closest('.revision-search')?.toggleAttribute('hidden', true);
  revisionPan.closest('.revision-pan')?.toggleAttribute('hidden', true);
  revisionList.classList.add('searching');
  revisionList.replaceChildren();
  revisionObserver?.disconnect();
  revisionObserver = null;
  revisionMoreButton = null;
  revisionStatus = null;

  headCount.textContent = 'loading';
  let response: ChangesResponse;
  try {
    response = await fetchJson<ChangesResponse>('/api/changes');
  } catch (error) {
    headCount.textContent = '';
    renderLoadFailure('the review queue', error);
    return;
  }
  // Every approval in one request rather than one per change: the queue needs them all to say
  // which stacks are ready, and a request per change would make an empty queue cheap and a
  // busy one slow.
  try {
    reviewAllApprovals = await fetchJson<Approval[]>('/api/approvals');
  } catch {
    reviewAllApprovals = [];
  }

  if (justMerged != null) {
    const stillListed = response.stacks.some((stack) => stack.bookmark === justMerged!.bookmark);
    const landed = document.createElement('p');
    landed.className = 'revision-status merged';
    landed.textContent = stillListed
      ? `Landed ${justMerged.bookmark} at ${short(justMerged.tip)}. It is still listed below because`
        + ' the viewer reads a snapshot of the repository, refreshed every couple of minutes.'
      : `Landed ${justMerged.bookmark} at ${short(justMerged.tip)}.`;
    revisionList.append(landed);
    if (!stillListed) justMerged = null;
  }

  const open = response.stacks.reduce((total, stack) => total + stack.changes.length, 0);
  headCount.textContent = `${open} open`;
  if (open === 0) {
    // An empty queue is the normal state, not a fault. Say what would fill it, because the
    // convention is the only thing that puts a change here.
    const empty = document.createElement('p');
    empty.className = 'revision-status';
    empty.textContent = 'Nothing is waiting for review. Push a change to a review/* bookmark.';
    revisionList.append(empty);
    content.innerHTML = '<p class="empty-state">No changes are open for review.</p>';
    changeHeading.innerHTML = '<p>Review</p>';
    return;
  }

  const buttons: HTMLButtonElement[] = [];
  const order: string[] = [];
  for (const stack of response.stacks) {
    revisionList.append(renderStackHeader(stack));
    for (const change of stack.changes) {
      const button = document.createElement('button');
      button.className = 'revision in-stack';
      const approved = approvalsOf(change.commit_id).length > 0;
      button.innerHTML = `
        <span class="revision-copy">
          <span class="revision-id">${escapeHtml(short(change.change_id))}</span>
          <strong>${escapeHtml(firstLine(change.description) || '(no description)')}</strong>
          <small>${escapeHtml(change.author_name)} · ${formatDateTime(change.authored_at)}</small>
          <span class="signals">
            <em class="${approved ? 'approved' : 'needs-review'}">${approved ? 'approved' : 'needs review'}</em>
            <em class="${change.patch_sets > 1 ? 'revised' : ''}">${change.patch_sets} patch set${change.patch_sets === 1 ? '' : 's'}</em>
            ${change.has_conflict ? '<em class="warning">conflict</em>' : ''}
          </span>
        </span>`;
      button.addEventListener('click', () => void selectChange(change.change_id, button));
      revisionList.append(button);
      buttons.push(button);
      order.push(change.change_id);
    }
  }

  // Open a change without a click: the one the URL asked for, else the first. A `?change=`
  // link is what a refused push prints, so it must land on that change and not on whatever
  // happens to be at the top of the queue.
  const asked = wanted == null ? -1 : order.findIndex((id) => id.startsWith(wanted));
  const index = asked === -1 ? 0 : asked;
  if (buttons[index] != null) await selectChange(order[index], buttons[index]);
  if (wanted != null && asked === -1) {
    // A change that is not open: landed already, abandoned, or a stale link. Say so, rather
    // than quietly showing a different one and letting someone approve the wrong change.
    const note = document.createElement('p');
    note.className = 'revision-status';
    note.textContent = `Change ${short(wanted)} is not open for review — it may have landed already.`;
    revisionList.prepend(note);
  }
}

function approvalsOf(commitId: string): Approval[] {
  return reviewAllApprovals.filter((approval) => approval.commit_id === commitId);
}

/// A stack's header: the bookmark, how much of it is approved, and the Merge button.
///
/// Merge lands the TIP, which lands everything beneath it — so the button lives here rather
/// than on a change, and is only enabled once every change in the stack is approved. That is
/// politeness, not enforcement: the gate is the update hook, and if this is wrong the push is
/// refused and says why.
function renderStackHeader(stack: Stack): HTMLElement {
  const ready = stack.changes.filter((c) => approvalsOf(c.commit_id).length > 0).length;
  const total = stack.changes.length;
  const mergeable = ready === total && stack.changes.every((c) => !c.has_conflict);

  const header = document.createElement('div');
  header.className = 'stack-header';
  header.innerHTML = `
    <div class="stack-name">
      <code>${escapeHtml(stack.bookmark)}</code>
      <span class="stack-count">${ready} of ${total} approved</span>
    </div>
    ${!reviewCanMerge ? '' : `
      <button type="button" class="stack-merge" ${mergeable ? '' : 'disabled'}
              title="${mergeable ? `Land ${escapeHtml(short(stack.tip))} on main` : 'Every change in the stack must be approved first'}">
        Merge
      </button>`}
  `;
  header.querySelector<HTMLButtonElement>('.stack-merge')
    ?.addEventListener('click', (event) => void mergeStack(stack, event.currentTarget as HTMLButtonElement));
  return header;
}

async function mergeStack(stack: Stack, button: HTMLButtonElement) {
  const label = button.textContent ?? 'Merge';
  button.disabled = true;
  button.textContent = 'Merging…';
  button.closest('.stack-header')?.querySelector('.stack-error')?.remove();
  try {
    await postJson<{ output: string }>('/api/merge', {
      bookmark: stack.bookmark,
      // The tip as it was when this was rendered. If a new patch set has been pushed since,
      // the push is refused rather than landing something nobody on this screen has read.
      tip: stack.tip,
    });
    // The viewer reads a SNAPSHOT of the repository, refreshed on a timer — so main has moved
    // on the canonical repository and this queue has not heard about it yet. Reloading alone
    // would redraw the stack exactly as it was and read as "nothing happened", which is the
    // worst possible answer to a button that just published to 31 repositories.
    justMerged = { bookmark: stack.bookmark, tip: stack.tip };
    await loadReviewQueue();
  } catch (error) {
    button.disabled = false;
    button.textContent = label;
    const message = document.createElement('pre');
    message.className = 'stack-error';
    // The update hook's refusal, verbatim: it names each unapproved change and links to it,
    // and summarising it would throw away the only actionable part.
    message.textContent = error instanceof Error ? error.message : String(error);
    button.closest('.stack-header')?.append(message);
  }
}

/// Show one change: its patch sets, and the diff of whichever is selected.
async function selectChange(changeId: string, button: HTMLButtonElement | null, patchSet?: string) {
  currentRevisionButton?.classList.remove('selected');
  currentRevisionButton = button;
  button?.classList.add('selected');

  let detail: ChangeDetail;
  try {
    detail = await fetchJson<ChangeDetail>(`/api/changes/${encodeURIComponent(changeId)}`);
  } catch (error) {
    renderLoadFailure(`change ${short(changeId)}`, error);
    return;
  }

  const latest = detail.patch_sets.find((set) => set.current)?.commit_id ?? detail.current.commit_id;
  const showing = patchSet ?? latest;
  currentRevision = detail.current;

  // Threads belong to the change, not to a patch set, so they are fetched once here and placed
  // against whichever version is being read. Loading them before the diff means the first item
  // to arrive already carries its comments instead of appearing bare and then twitching.
  reviewChangeId = detail.change_id;
  reviewPatchSet = showing;
  await Promise.all([loadThreads(detail.change_id), loadApprovals(detail.change_id)]);
  renderChangeHeading(detail, showing);

  // The diff of the selected patch set, through the existing stream. Reading an OLD patch set
  // is the point of keeping them, so this is not restricted to the current one.
  const generation = ++selectionGeneration;
  fileGeneration += 1;
  diffController?.abort();
  diffController = null;
  diffParser?.terminate();
  diffParser = null;
  cleanContentRenderers();
  content.textContent = 'Loading comparison…';
  fileHeading.textContent = 'Loading changed files…';
  const controller = new AbortController();
  diffController = controller;
  try {
    await streamDiff(`/api/revisions/${showing}/diff`, controller.signal, async (event) => {
      if (generation !== selectionGeneration) return;
      if (event.type === 'metadata') {
        fileHeading.textContent = `${event.paths.length.toLocaleString()} changed files`;
        const prepared = prepareFileTreeInput(event.paths, { flattenEmptyDirectories: false });
        renderFileTree(prepared, 'open', (path) => scrollToDiff(path, 'smooth-auto'));
        setupCodeView(event.paths);
        diffParser = new DiffParser();
        return;
      }
      if (event.type === 'error') throw new Error(event.error);
      const fileDiff = await diffParser!.parse(event);
      if (generation !== selectionGeneration) return;
      fileSides.set(event.path, { before: event.before, after: event.after });
      const item = codeViewItem(event.path, fileDiff, nextVersion(event.path));
      diffItems[event.index] = item;
      loadedDiffPaths.add(event.path);
      codeView?.updateItem(item);
    });
  } catch (error) {
    if (controller.signal.aborted) return;
    if (generation !== selectionGeneration) return;
    renderLoadFailure(`the diff of ${short(showing)}`, error);
  } finally {
    if (diffController === controller) diffController = null;
  }
}

/// The heading for a change, with a control for choosing which patch set to read.
function renderChangeHeading(detail: ChangeDetail, showing: string) {
  const sets = detail.patch_sets;
  changeHeading.innerHTML = `
    <div><code>${escapeHtml(short(detail.change_id))}</code><span>change</span></div>
    <h2>${escapeHtml(firstLine(detail.current.description) || '(no description)')}</h2>
    <p>
      ${detail.bookmark == null ? '' : `on <code>${escapeHtml(detail.bookmark)}</code> · `}
      by ${escapeHtml(detail.current.author_name)}
      ${reviewWritable ? ` · <span class="thread-count">${escapeHtml(threadCountLabel())}</span>` : ''}
    </p>
    ${sets.length === 0 ? '' : `
      <div class="patch-sets" role="group" aria-label="Patch sets">
        ${sets.map((set) => `
          <button type="button" data-patch-set="${escapeHtml(set.commit_id)}"
                  class="${set.commit_id === showing ? 'selected' : ''}">
            ${set.number}${set.current ? '' : ''}
          </button>`).join('')}
        ${sets.length > 1 && showing !== (sets.find((s) => s.current)?.commit_id ?? '')
          ? '<span class="patch-note">viewing a superseded version</span>' : ''}
      </div>`}
    ${renderApprovalControl(showing)}
  `;
  for (const button of changeHeading.querySelectorAll<HTMLButtonElement>('[data-patch-set]')) {
    button.addEventListener('click', () => {
      void selectChange(detail.change_id, currentRevisionButton, button.dataset.patchSet);
    });
  }
  const approve = changeHeading.querySelector<HTMLButtonElement>('[data-approve]');
  approve?.addEventListener('click', () => void toggleApproval(detail, showing, approve));
}

/// The approve control, describing the state of the patch set actually on screen.
///
/// Deliberately about `showing` rather than about "the change": an approval says someone read
/// this exact text, and hooks/update matches on the commit id. Approving while looking at a
/// superseded version approves that version and will not let the current one land -- so the
/// control says which one it means, rather than leaving it to be discovered at push time.
function renderApprovalControl(showing: string): string {
  if (!reviewWritable) return '';
  const here = reviewApprovals.filter((approval) => approval.commit_id === showing);
  const elsewhere = reviewApprovals.length - here.length;
  const mine = here.some((approval) => approval.approved_by === reviewIdentity);
  return `
    <div class="approval">
      <button type="button" data-approve class="${mine ? 'withdraw' : ''}">
        ${mine ? 'Withdraw approval' : 'Approve this patch set'}
      </button>
      <span class="approval-state ${here.length === 0 ? '' : 'approved'}">
        ${here.length === 0
          ? 'Not approved'
          : `Approved by ${here.map((a) => escapeHtml(a.approved_by)).join(', ')}`}
      </span>
      ${elsewhere === 0 ? '' : `<span class="approval-note">${elsewhere} approval${elsewhere === 1 ? '' : 's'} of another patch set, which will not let this one land</span>`}
      ${reviewGated ? '' : '<span class="approval-note">this instance records approvals but does not gate pushes</span>'}
    </div>
  `;
}

async function toggleApproval(detail: ChangeDetail, showing: string, button: HTMLButtonElement) {
  const mine = reviewApprovals.some(
    (approval) => approval.commit_id === showing && approval.approved_by === reviewIdentity,
  );
  // Say what is happening. A disabled button that only changes back when the answer arrives
  // is indistinguishable from a dead one, and the first person to use this pressed it three
  // times over thirty seconds because nothing said it had been heard. Every server-side
  // measurement of this request is under 110ms, so the delay was somewhere between browser
  // and box — which makes it exactly the case the UI has to survive rather than assume away.
  const label = button.textContent ?? '';
  button.disabled = true;
  button.textContent = mine ? 'Withdrawing…' : 'Approving…';
  const slow = window.setTimeout(() => {
    button.textContent = mine ? 'Still withdrawing…' : 'Still approving…';
  }, 3000);
  try {
    const response = await postJson<ApprovalsResponse>(
      `/api/changes/${encodeURIComponent(detail.change_id)}/approvals`,
      { commit_id: showing, approved: !mine },
    );
    reviewApprovals = response.approvals;
    reviewGated = response.gated;
    renderChangeHeading(detail, showing);
  } catch (error) {
    button.disabled = false;
    button.textContent = label;
    const message = document.createElement('p');
    message.className = 'thread-error';
    message.textContent = error instanceof Error ? error.message : String(error);
    button.closest('.approval')?.append(message);
  } finally {
    window.clearTimeout(slow);
  }
}

async function setMode(mode: ViewMode) {
  if (mode === currentMode) return;
  const leavingReview = currentMode === 'review';
  currentMode = mode;
  syncModeButtons();
  if (mode === 'review') {
    await loadReviewQueue();
    return;
  }
  if (leavingReview) {
    // Coming back from the queue, the revisions panel is showing changes; rebuild it.
    panelTitle.textContent = 'Revisions';
    revisionSearch.closest('.revision-search')?.toggleAttribute('hidden', false);
    await loadRevisionPage(true);
  }
  if (currentRevision != null) {
    setViewUrl(currentRevision, null, false);
    await loadRevision(currentRevision, null);
  }
}

function syncModeButtons() {
  for (const button of modeButtons) {
    const selected = button.dataset.mode === currentMode;
    button.classList.toggle('selected', selected);
    button.setAttribute('aria-pressed', String(selected));
  }
}

/// The graph column is a fixed-width window onto the full topology; the slider translates the
/// drawing behind it. The range is simply how much of the drawing does not fit in the window.
function syncRevisionPanRange() {
  const sample = revisionList.querySelector('.revision-graph');
  const maximum = sample == null ? 0 : Math.max(0, graphWidth - sample.clientWidth);
  revisionPan.max = String(maximum);
  revisionPan.disabled = maximum === 0;
  if (Number(revisionPan.value) > maximum) revisionPan.value = String(maximum);
  applyGraphPan();
}

function applyGraphPan() {
  revisionList.style.setProperty('--graph-pan', `${-Number(revisionPan.value)}px`);
}

async function selectRevision(revision: Revision, button: HTMLButtonElement | null, updateHistory: boolean) {
  currentRevision = revision;
  currentRevisionButton?.classList.remove('selected');
  currentRevisionButton = button;
  button?.classList.add('selected');
  if (updateHistory) {
    currentMode = defaultModeFor(revision);
    syncModeButtons();
    setViewUrl(revision, null, true);
  }
  const requestedPath = currentMode === 'changes' ? readUrlState().path : null;
  await loadRevision(revision, requestedPath);
}

async function loadRevision(revision: Revision, requestedPath: string | null = null) {
  // Browse and Changes read a revision, not a change under review. Comments belong to a change,
  // so leaving review mode has to put the gutter affordance away with them — offering "+" on a
  // revision that is not being reviewed would open a composer with nowhere to post.
  forgetReviewThreads();
  const generation = ++selectionGeneration;
  fileGeneration += 1;
  diffController?.abort();
  diffController = null;
  diffParser?.terminate();
  diffParser = null;
  cleanContentRenderers();
  content.scrollTop = 0;
  renderRevisionHeading(revision);

  if (currentMode === 'browse') {
    fileHeading.textContent = 'Loading files…';
    renderRevisionOverview(revision);
    let result: TreeResponse;
    try {
      result = await fetchJson<TreeResponse>(`/api/revisions/${revision.commit_id}/tree`);
    } catch (error) {
      if (generation !== selectionGeneration) return;
      fileHeading.textContent = 'Files';
      renderLoadFailure(`the file list for ${short(revision.change_id)}`, error);
      return;
    }
    if (generation !== selectionGeneration) return;
    const preparedInput = prepareFileTreeInput(result.paths.map((entry) => entry.path), {
      flattenEmptyDirectories: false,
    });
    operation.textContent = `operation ${short(result.operation_id)}`;
    fileHeading.textContent = `${result.paths.length.toLocaleString()} files`;
    renderFileTree(preparedInput, 1, (path) => void showFile(revision, path));
    return;
  }

  fileHeading.textContent = 'Loading changed files…';
  content.textContent = 'Loading comparison…';
  const controller = new AbortController();
  diffController = controller;
  let metadataReceived = false;
  let processedFiles = 0;
  try {
    await streamDiff(
      `/api/revisions/${revision.commit_id}/diff${requestedPath == null ? '' : `?path=${encodeURIComponent(requestedPath)}`}`,
      controller.signal,
      async (event) => {
        if (generation !== selectionGeneration) return;
        if (event.type === 'metadata') {
          metadataReceived = true;
          operation.textContent = `operation ${short(event.operation_id)}`;
          fileHeading.textContent = `${event.paths.length.toLocaleString()} changed files`;
          const preparedInput = prepareFileTreeInput(event.paths, { flattenEmptyDirectories: false });
          renderFileTree(preparedInput, 'open', (path) => void selectDiffPath(path));
          setupCodeView(event.paths);
          diffParser = new DiffParser();
          if (requestedPath != null) scrollToDiff(requestedPath, 'instant');
          return;
        }
        if (event.type === 'error') throw new Error(event.error);
        const fileDiff = await diffParser!.parse(event);
        if (generation !== selectionGeneration) return;
        const item = codeViewItem(event.path, fileDiff, 1);
        diffItems[event.index] = item;
        loadedDiffPaths.add(event.path);
        codeView?.updateItem(item);
        processedFiles += 1;
        fileHeading.textContent = `${processedFiles.toLocaleString()} / ${diffItems.length.toLocaleString()} changed files`;
        if (event.path === requestedPath) scrollToDiff(event.path, 'instant');
        if (processedFiles % 4 === 0) await nextFrame();
      },
    );
  } catch (error) {
    if (controller.signal.aborted) return;
    throw error;
  } finally {
    if (diffController === controller) diffController = null;
  }
  if (generation !== selectionGeneration) return;
  if (!metadataReceived) throw new Error('diff stream did not include metadata');
  fileHeading.textContent = `${diffItems.length.toLocaleString()} changed files`;
  if (diffItems.length === 0) content.innerHTML = '<p class="empty-state">This revision has no file changes.</p>';
}

/// Fill the detail pane with the revision itself, until a file is chosen.
///
/// It used to say "Select a file to view its contents." inside a dashed box occupying most of
/// the window, while the commit message BODY — the part carrying why the change was made — was
/// displayed nowhere at all. The heading shows its first line and the rest was unreachable
/// without leaving the viewer.
function renderRevisionOverview(revision: Revision) {
  const lines = revision.description.split('\n');
  const body = lines.slice(1).join('\n').trim();
  const chips: string[] = [];
  if (revision.working_copy) chips.push('<em>working copy</em>');
  if (revision.is_head) chips.push('<em class="head">head</em>');
  if (revision.divergent) chips.push('<em class="warning">divergent</em>');
  if (revision.has_conflict) chips.push('<em class="warning">conflict</em>');
  for (const name of revision.bookmarks) chips.push(`<em>${escapeHtml(name)}</em>`);

  content.innerHTML = `
    <section class="revision-overview">
      ${body === '' ? '' : `<pre class="revision-body">${escapeHtml(body)}</pre>`}
      <dl class="revision-facts">
        <div><dt>change</dt><dd><code>${escapeHtml(revision.change_id)}</code></dd></div>
        <div><dt>commit</dt><dd><code>${escapeHtml(revision.commit_id)}</code></dd></div>
        <div><dt>author</dt><dd>${escapeHtml(revision.author_name)}
          &lt;${escapeHtml(revision.author_email)}&gt;</dd></div>
        <div><dt>authored</dt><dd>${escapeHtml(formatDateTime(revision.authored_at))}</dd></div>
        <div><dt>parents</dt><dd>${
          revision.parent_commit_ids.length === 0
            ? '<span class="muted">none — this is a root</span>'
            : revision.parent_commit_ids.map((id) => `<code>${escapeHtml(short(id))}</code>`).join(' ')
        }</dd></div>
      </dl>
      ${chips.length === 0 ? '' : `<div class="signals">${chips.join('')}</div>`}
      <p class="revision-hint">Select a file from the tree to read it at this revision.</p>
    </section>`;
}

/// Date and TIME. A repository where a day's work is thirty commits shows thirty identical
/// dates, which orders nothing.
function formatDateTime(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString(undefined, {
    year: 'numeric', month: 'short', day: 'numeric',
    hour: '2-digit', minute: '2-digit',
  });
}

function renderRevisionHeading(revision: Revision) {
  changeHeading.innerHTML = `
    <div><code>${escapeHtml(short(revision.change_id))}</code><span>${currentMode === 'browse' ? 'revision' : 'changes'}</span></div>
    <h2>${escapeHtml(firstLine(revision.description) || '(no description)')}</h2>
    <p>commit <code>${escapeHtml(short(revision.commit_id))}</code> by ${escapeHtml(revision.author_name)}</p>
  `;
}

function renderFileTree(
  preparedInput: FileTreePreparedInput,
  initialExpansion: 'open' | number,
  onSelect: (path: string) => void,
) {
  tree?.cleanUp();
  fileTreeContainer.replaceChildren();
  tree = new FileTree({
    preparedInput,
    initialExpansion,
    initialVisibleRowCount: 40,
    search: true,
    density: 'compact',
    onSelectionChange(paths) {
      const path = paths[0];
      if (path == null || tree?.getItem(path)?.isDirectory() !== false) return;
      onSelect(path);
    },
  });
  tree.render({ containerWrapper: fileTreeContainer });
  applyTreeTheme();
}

async function showFile(revision: Revision, path: string) {
  const generation = ++fileGeneration;
  renderedFile?.cleanUp();
  renderedFile = null;
  content.innerHTML = `<p class="empty-state">Loading <code>${escapeHtml(path)}</code>…</p>`;
  let result: FileResponse;
  try {
    result = await fetchJson<FileResponse>(
      `/api/revisions/${revision.commit_id}/file?path=${encodeURIComponent(path)}`,
    );
  } catch (error) {
    if (generation !== fileGeneration) return;
    renderLoadFailure(path, error);
    return;
  }
  if (generation !== fileGeneration || revision.commit_id !== currentRevision?.commit_id || currentMode !== 'browse') return;
  operation.textContent = `operation ${short(result.operation_id)}`;
  content.replaceChildren();

  if (result.binary) {
    content.innerHTML = `<section class="file-message"><h3>${escapeHtml(path)}</h3><p>Binary or oversized content is not rendered.</p></section>`;
    return;
  }
  if (result.conflicted) {
    renderConflict(path, result.conflict);
    return;
  }

  const mount = document.createElement('section');
  mount.className = 'file-view';
  content.append(mount);
  renderedFile = new FileViewer({
    theme: { dark: 'pierre-dark', light: 'pierre-light' },
    themeType: pierreThemeType(),
    overflow: 'wrap',
  });
  renderedFile.render({
    file: { name: path, contents: result.contents ?? '' },
    containerWrapper: mount,
  });
}

async function selectDiffPath(path: string) {
  const revision = currentRevision;
  if (revision == null || currentMode !== 'changes') return;
  setViewUrl(revision, path, true);
  if (loadedDiffPaths.has(path)) {
    scrollToDiff(path, 'smooth-auto');
    return;
  }
  await loadRevision(revision, path);
}

function scrollToDiff(path: string, behavior: 'instant' | 'smooth-auto') {
  codeView?.scrollTo({ type: 'item', id: path, align: 'start', behavior });
}

function setupCodeView(paths: string[]) {
  cleanContentRenderers();
  content.replaceChildren();
  loadedDiffPaths = new Set();
  diffItems = paths.map((path) => codeViewPlaceholder(path));
  if (paths.length === 0) return;
  codeView = new CodeView(codeViewOptions());
  codeView.setup(content);
  codeView.setItems(diffItems);
}

function codeViewPlaceholder(path: string): CodeViewItem<ThreadAnnotation> {
  return {
    id: path,
    type: 'diff',
    fileDiff: parseDiffFromFile(
      { name: path, contents: '' },
      { name: path, contents: '' },
    ),
    version: 0,
    collapsed: true,
  };
}

function codeViewItem(
  path: string,
  fileDiff: FileDiffMetadata,
  version: number,
): CodeViewItem<ThreadAnnotation> {
  return {
    id: path,
    type: 'diff',
    fileDiff,
    annotations: annotationsFor(path),
    version,
    collapsed: false,
  };
}

function codeViewOptions(): CodeViewOptions<ThreadAnnotation> {
  const reviewing = reviewChangeId != null && reviewWritable;
  return {
    theme: { dark: 'pierre-dark', light: 'pierre-light' },
    themeType: pierreThemeType(),
    diffStyle: 'unified',
    overflow: 'wrap',
    stickyHeaders: true,
    layout: { paddingTop: 22, paddingBottom: 22, gap: 18 },
    renderAnnotation: (annotation) => renderAnnotation(annotation.metadata),
    // The library's own hover "+" in the gutter. Only offered while a change is open and this
    // instance can actually store what gets typed.
    enableGutterUtility: reviewing,
    onGutterUtilityClick: reviewing
      ? (range, context) => openDraft(context.item.id, range)
      : undefined,
  };
}

// ---------------------------------------------------------------------------
// Review threads.
//
// A thread is anchored to a change and to the text of a line, and is rendered against whichever
// patch set is on screen — which may not be the one it was written against. threads.ts decides
// where each one goes and how sure it is; everything here is presentation and writing.
// ---------------------------------------------------------------------------

async function loadThreads(changeId: string) {
  try {
    reviewThreads = await fetchJson<Thread[]>(`/api/changes/${encodeURIComponent(changeId)}/threads`);
    reviewWritable = true;
  } catch {
    // An instance started without `--review-db` refuses to read threads for the same reason it
    // refuses to write them. That is a configuration, not a fault: show the diff, offer nothing.
    reviewThreads = [];
    reviewWritable = false;
  }
  draft = null;
  draftBody = '';
  replyBodies.clear();
  annotationCache.clear();
}

async function loadApprovals(changeId: string) {
  try {
    const response = await fetchJson<ApprovalsResponse>(
      `/api/changes/${encodeURIComponent(changeId)}/approvals`,
    );
    reviewApprovals = response.approvals;
    reviewGated = response.gated;
  } catch {
    reviewApprovals = [];
    reviewGated = false;
  }
}

function forgetReviewThreads() {
  reviewChangeId = null;
  reviewPatchSet = null;
  reviewThreads = [];
  reviewApprovals = [];
  reviewGated = false;
  reviewWritable = false;
  draft = null;
  draftBody = '';
  replyBodies.clear();
  annotationCache.clear();
}

/// Every annotation on one file: its threads, placed, plus the composer if it is open here.
function annotationsFor(path: string): DiffLineAnnotation<ThreadAnnotation>[] {
  const annotations: DiffLineAnnotation<ThreadAnnotation>[] = [];
  const sides = fileSides.get(path);
  if (sides != null) {
    for (const placed of placeThreads(reviewThreads, path, sides)) {
      annotations.push({
        side: placed.side,
        lineNumber: placed.line,
        metadata: cachedAnnotation(`thread:${placed.thread.id}:${placed.line}`, {
          path,
          side: placed.side,
          line: placed.line,
          placed,
        }),
      });
    }
  }
  if (draft != null && draft.path === path) {
    annotations.push({ side: draft.side, lineNumber: draft.line, metadata: draft.annotation });
  }
  return annotations;
}

/// Hand back the same metadata object for the same annotation.
///
/// @pierre/diffs compares metadata by identity to decide whether a card needs re-rendering, so
/// a fresh object every render means a rebuilt DOM on every scroll — and a reply box that loses
/// focus and its selection while being typed into.
function cachedAnnotation(key: string, value: ThreadAnnotation): ThreadAnnotation {
  const existing = annotationCache.get(key);
  if (existing != null) return existing;
  annotationCache.set(key, value);
  return value;
}

/// Drop a thread's cached metadata so its card is rebuilt with what the server just returned.
function invalidateThread(threadId: number) {
  for (const key of [...annotationCache.keys()]) {
    if (key.startsWith(`thread:${threadId}:`)) annotationCache.delete(key);
  }
}

function nextVersion(path: string): number {
  const version = (itemVersions.get(path) ?? 0) + 1;
  itemVersions.set(path, version);
  return version;
}

/// Re-render one file's annotations. Bumping the version is what makes `updateItem` do anything.
function refreshAnnotations(path: string) {
  const index = diffItems.findIndex((item) => item.id === path);
  if (index === -1) return;
  const item = diffItems[index];
  if (item.type !== 'diff') return;
  const next: CodeViewItem<ThreadAnnotation> = {
    ...item,
    annotations: annotationsFor(path),
    version: nextVersion(path),
  };
  diffItems[index] = next;
  codeView?.updateItem(next);
}

/// Open the composer where the "+" was clicked.
///
/// A drag over the gutter selects a range; a comment is anchored to a single line, so the start
/// is what counts. Anchoring to the first line of a selection is what Gerrit does too.
function openDraft(path: string, range: { start: number; side?: Side }) {
  const previous = draft?.path;
  const side = range.side ?? 'additions';
  draft = { path, side, line: range.start, annotation: { path, side, line: range.start } };
  draftBody = '';
  if (previous != null && previous !== path) refreshAnnotations(previous);
  refreshAnnotations(path);
}

function closeDraft() {
  const path = draft?.path;
  draft = null;
  draftBody = '';
  if (path != null) refreshAnnotations(path);
}

function renderAnnotation(metadata: ThreadAnnotation | undefined): HTMLElement | undefined {
  if (metadata == null) return undefined;
  return metadata.placed == null
    ? renderComposer(metadata)
    : renderThreadCard(metadata.placed, metadata.path);
}

function renderThreadCard(placed: PlacedThread, path: string): HTMLElement {
  const { thread } = placed;
  const card = document.createElement('div');
  card.className = `review-thread${thread.resolved ? ' resolved' : ''} ${placed.confidence}`;
  const note = placementNote(placed);
  card.innerHTML = `
    <div class="thread-meta">
      <span class="thread-state">${thread.resolved ? 'Resolved' : 'Open'}</span>
      ${note == null ? '' : `<span class="thread-note">${escapeHtml(note)}</span>`}
    </div>
    <ol class="thread-comments">
      ${thread.comments.map((comment) => `
        <li>
          <div class="comment-meta">
            <strong>${escapeHtml(comment.author)}</strong>
            <time>${escapeHtml(formatUnix(comment.created_at))}</time>
            ${comment.patch_set_commit_id === reviewPatchSet ? '' :
              `<span class="comment-elsewhere" title="${escapeHtml(comment.patch_set_commit_id)}">on another patch set</span>`}
          </div>
          <p>${escapeHtml(comment.body)}</p>
        </li>`).join('')}
    </ol>
    ${!reviewWritable ? '' : `
      <form class="thread-reply">
        <textarea rows="2" placeholder="Reply" aria-label="Reply to this thread"></textarea>
        <div class="thread-actions">
          <button type="submit">Reply</button>
          <button type="button" data-resolve>${thread.resolved ? 'Reopen' : 'Resolve'}</button>
        </div>
      </form>`}
  `;
  if (!reviewWritable) return card;

  const form = card.querySelector<HTMLFormElement>('.thread-reply')!;
  const textarea = form.querySelector('textarea')!;
  // Restored rather than assumed empty: scrolling this file out of view unmounts the card, and
  // an unsent reply that evaporates because you looked at something else is a lost remark.
  textarea.value = replyBodies.get(thread.id) ?? '';
  textarea.addEventListener('input', () => replyBodies.set(thread.id, textarea.value));
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    void submitReply(thread.id, path, textarea.value, form);
  });
  form.querySelector<HTMLButtonElement>('[data-resolve]')!.addEventListener('click', () => {
    void submitResolve(thread.id, path, !thread.resolved, form);
  });
  return card;
}

function renderComposer(draftAnnotation: { path: string; side: Side; line: number }): HTMLElement {
  const card = document.createElement('div');
  card.className = 'review-thread composing';
  card.innerHTML = `
    <div class="thread-meta">
      <span class="thread-state">New comment</span>
      <span class="thread-note">on the ${draftAnnotation.side === 'additions' ? 'new' : 'old'} line ${draftAnnotation.line}</span>
    </div>
    <form class="thread-reply">
      <textarea rows="3" placeholder="Leave a comment" aria-label="New comment"></textarea>
      <div class="thread-actions">
        <button type="submit">Comment</button>
        <button type="button" data-cancel>Cancel</button>
      </div>
    </form>
  `;
  const form = card.querySelector<HTMLFormElement>('.thread-reply')!;
  const textarea = form.querySelector('textarea')!;
  textarea.value = draftBody;
  textarea.addEventListener('input', () => { draftBody = textarea.value; });
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    void submitThread(textarea.value, form);
  });
  form.querySelector<HTMLButtonElement>('[data-cancel]')!.addEventListener('click', closeDraft);
  // Deferred: the card is appended to the document by the caller, and focusing a node that is
  // not in the tree yet does nothing.
  queueMicrotask(() => textarea.focus());
  return card;
}

async function submitThread(body: string, form: HTMLFormElement) {
  if (draft == null || reviewChangeId == null || reviewPatchSet == null) return;
  if (body.trim() === '') return;
  const sides = fileSides.get(draft.path);
  const contents = sides == null ? null : (draft.side === 'additions' ? sides.after : sides.before);
  if (contents == null) {
    showFormError(form, 'this line is not in the version being read');
    return;
  }
  // Captured here, in the browser, from the text the reader is looking at. The server stores the
  // fingerprint without knowing what it means, which is what keeps relocation out of Rust.
  const anchor = captureAnchor(draft.path, draft.side, contents, draft.line);
  const path = draft.path;
  await withBusyForm(form, async () => {
    const thread = await postJson<Thread>(
      `/api/changes/${encodeURIComponent(reviewChangeId!)}/threads`,
      {
        path: anchor.path,
        side: anchor.side,
        line: anchor.line,
        fingerprint: anchor.fingerprint,
        context: JSON.stringify(anchor.context),
        body,
        patch_set_commit_id: reviewPatchSet,
      },
    );
    reviewThreads = [...reviewThreads, thread];
    draft = null;
    draftBody = '';
    refreshAnnotations(path);
    updateThreadCount();
  });
}

async function submitReply(threadId: number, path: string, body: string, form: HTMLFormElement) {
  if (body.trim() === '' || reviewPatchSet == null) return;
  await withBusyForm(form, async () => {
    const updated = await postJson<Thread>(`/api/threads/${threadId}/comments`, {
      body,
      patch_set_commit_id: reviewPatchSet,
    });
    replaceThread(updated);
    replyBodies.delete(threadId);
    refreshAnnotations(path);
  });
}

async function submitResolve(threadId: number, path: string, resolved: boolean, form: HTMLFormElement) {
  await withBusyForm(form, async () => {
    const updated = await postJson<Thread>(`/api/threads/${threadId}/resolve`, { resolved });
    replaceThread(updated);
    refreshAnnotations(path);
    updateThreadCount();
  });
}

function replaceThread(updated: Thread) {
  reviewThreads = reviewThreads.map((thread) => (thread.id === updated.id ? updated : thread));
  invalidateThread(updated.id);
}

/// Run a form submission with its controls disabled, and surface a failure in the form itself.
///
/// A write that fails silently is the worst outcome here: the comment looks sent, and the person
/// it was addressed to never sees it.
async function withBusyForm(form: HTMLFormElement, body: () => Promise<void>) {
  const controls = [...form.querySelectorAll<HTMLButtonElement | HTMLTextAreaElement>('button, textarea')];
  for (const control of controls) control.disabled = true;
  form.querySelector('.thread-error')?.remove();
  try {
    await body();
  } catch (error) {
    showFormError(form, error instanceof Error ? error.message : String(error));
  } finally {
    for (const control of controls) control.disabled = false;
  }
}

function showFormError(form: HTMLFormElement, message: string) {
  form.querySelector('.thread-error')?.remove();
  const paragraph = document.createElement('p');
  paragraph.className = 'thread-error';
  paragraph.textContent = message;
  form.append(paragraph);
}

/// Keep the count in the change heading honest as threads are opened and resolved.
function updateThreadCount() {
  const badge = changeHeading.querySelector('.thread-count');
  if (badge == null) return;
  badge.textContent = threadCountLabel();
}

function threadCountLabel(): string {
  if (reviewThreads.length === 0) return 'no comments';
  const open = unresolvedCount(reviewThreads);
  const total = `${reviewThreads.length} comment thread${reviewThreads.length === 1 ? '' : 's'}`;
  return open === 0 ? `${total}, all resolved` : `${total}, ${open} open`;
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error(await readError(response));
  return response.json() as Promise<T>;
}

function formatUnix(seconds: number): string {
  return formatDateTime(new Date(seconds * 1000).toISOString());
}

function cleanContentRenderers() {
  renderedFile?.cleanUp();
  renderedFile = null;
  codeView?.cleanUp();
  codeView = null;
  diffItems = [];
  loadedDiffPaths.clear();
  // Both are keyed by path and describe the items just thrown away. The threads themselves are
  // not cleared: they belong to the change, and switching patch sets re-places the same ones.
  fileSides.clear();
  itemVersions.clear();
}

class DiffParser {
  private readonly worker = new Worker(new URL('./diff-worker.ts', import.meta.url), { type: 'module' });
  private readonly pending = new Map<number, {
    resolve: (fileDiff: FileDiffMetadata) => void;
    reject: (error: Error) => void;
  }>();
  private nextId = 0;

  constructor() {
    this.worker.addEventListener('message', (message: MessageEvent<{
      id: number;
      fileDiff?: FileDiffMetadata;
      error?: string;
    }>) => {
      const pending = this.pending.get(message.data.id);
      if (pending == null) return;
      this.pending.delete(message.data.id);
      if (message.data.fileDiff != null) pending.resolve(message.data.fileDiff);
      else pending.reject(new Error(message.data.error ?? 'diff parser failed'));
    });
    this.worker.addEventListener('error', (event) => {
      for (const pending of this.pending.values()) pending.reject(new Error(event.message));
      this.pending.clear();
    });
  }

  parse(file: FileChange): Promise<FileDiffMetadata> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.worker.postMessage({ id, file });
    });
  }

  terminate() {
    this.worker.terminate();
    for (const pending of this.pending.values()) pending.reject(new DOMException('Aborted', 'AbortError'));
    this.pending.clear();
  }
}

function readThemePreference(): ThemePreference {
  const value = localStorage.getItem('cresset-view-theme');
  return value === 'light' || value === 'dark' ? value : 'auto';
}

function applyTheme(preference: ThemePreference) {
  document.documentElement.dataset.theme = preference;
  applyTreeTheme();
  const themeType = pierreThemeType();
  renderedFile?.setThemeType(themeType);
  codeView?.setOptions(codeViewOptions());
}

function applyTreeTheme() {
  const colorScheme = themePreference === 'auto' ? 'light dark' : themePreference;
  tree?.getFileTreeContainer()?.style.setProperty('color-scheme', colorScheme);
}

function renderRevisionGraph(row: GraphRow, revision: Revision, width: number, laneGap: number): string {
  const paths = row.segments.map((segment) => {
    const fromX = graphLaneX(segment.fromLane, laneGap);
    const toX = graphLaneX(segment.toLane, laneGap);
    const path = fromX === toX
      ? `M ${fromX} ${segment.fromY} L ${toX} ${segment.toY}`
      : `M ${fromX} ${segment.fromY} C ${fromX} 72, ${toX} 72, ${toX} ${segment.toY}`;
    return `<path class="graph-line lane-${segment.colorLane % 8}" d="${path}" />`;
  }).join('');
  const nodeClasses = [
    'graph-node',
    `lane-${row.lane % 8}`,
    revision.working_copy ? 'working-copy' : '',
    revision.is_head ? 'head' : '',
    revision.divergent ? 'divergent' : '',
    revision.has_conflict ? 'conflict' : '',
  ].filter(Boolean).join(' ');

  return `
    <span class="revision-graph" aria-hidden="true">
      <span class="graph-pan-layer">
        <svg viewBox="0 0 ${width} 100" preserveAspectRatio="none">${paths}</svg>
        <span class="${nodeClasses}" style="left: ${graphLaneX(row.lane, laneGap)}px"></span>
        ${revision.is_head ? `<span class="graph-head-label lane-${row.lane % 8}" style="left: ${graphLaneX(row.lane, laneGap) + 10}px">head</span>` : ''}
      </span>
    </span>
  `;
}

function pierreThemeType(): ThemeTypes {
  return themePreference === 'auto' ? 'system' : themePreference;
}

/// Fetch a revision by id, or `null` if the repository no longer has it.
///
/// Links to revisions that later vanish are normal here, not an edge case. A conflict link is
/// the clearest example: `sync/conflict/*` is deleted the moment the conflict is resolved, so
/// every conflict link in Telegram and in this viewer's own banner goes stale by design as soon
/// as someone does the thing it asked for. Reloading one used to throw an unhandled rejection
/// and leave a blank page.
async function fetchRevisionOrNull(id: string): Promise<Revision | null> {
  try {
    return await fetchJson<Revision>(`/api/revisions/${encodeURIComponent(id)}`);
  } catch {
    return null;
  }
}

/// Say that a revision is gone, and leave the viewer usable.
///
/// Deliberately does NOT silently substitute a different revision: the reader followed a link to
/// something specific, and quietly showing them something else would be worse than saying so.
/// The revision list beside it is loaded, so the next click still works.
function renderMissingRevision(id: string) {
  fileHeading.textContent = 'Files';
  changeHeading.innerHTML = '<p>No revision selected.</p>';
  content.innerHTML = `
    <section class="file-message">
      <h3>That revision is no longer in this repository</h3>
      <p><code>${escapeHtml(short(id))}</code> could not be resolved. Conflict links stop
      resolving once the conflict is resolved, and abandoned commits stop resolving once they
      are rewritten — both are normal. Pick a revision from the list to carry on.</p>
    </section>`;
}

async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  if (!response.ok) throw new Error(await readError(response));
  return response.json() as Promise<T>;
}

/// The server's message for a failed response, unwrapped from its JSON envelope.
///
/// Errors used to reach the UI as the raw body — `{"error":"requested path is not changed in
/// this revision"}` — which is a readable sentence wearing punctuation that says "internal".
async function readError(response: Response): Promise<string> {
  const body = await response.text().catch(() => '');
  try {
    const parsed = JSON.parse(body) as { error?: unknown };
    if (typeof parsed.error === 'string' && parsed.error !== '') return parsed.error;
  } catch {
    // Not JSON. The raw body, or the status, is still better than nothing.
  }
  return body.trim() !== '' ? body.trim() : `${response.status} ${response.statusText}`;
}

/// Show a failure in the content pane instead of leaving the last "Loading…" on screen.
///
/// Every load path used to rethrow into a `void`-ed call, so a failed fetch became an unhandled
/// rejection: the console got a stack trace and the pane sat at "Loading comparison…" for ever.
/// A viewer that stops without saying so is worse than one that says it cannot do the thing,
/// because the reader has no way to tell it apart from slow.
function renderLoadFailure(what: string, error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  content.innerHTML = `
    <section class="file-message">
      <h3>Could not load ${escapeHtml(what)}</h3>
      <p>${escapeHtml(message)}</p>
    </section>`;
}

async function streamDiff(
  url: string,
  signal: AbortSignal,
  onEvent: (event: DiffEvent) => void | Promise<void>,
) {
  const response = await fetch(url, { signal });
  if (!response.ok) throw new Error(await response.text());
  if (response.body == null) throw new Error('diff response cannot be streamed');

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffered = '';
  while (true) {
    const { done, value } = await reader.read();
    buffered += decoder.decode(value, { stream: !done });
    const lines = buffered.split('\n');
    buffered = lines.pop() ?? '';
    for (const line of lines) {
      if (line !== '') await onEvent(JSON.parse(line) as DiffEvent);
    }
    if (done) break;
  }
  if (buffered.trim() !== '') await onEvent(JSON.parse(buffered) as DiffEvent);
}

function nextFrame(): Promise<void> {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

function readUrlState(): { revision: string | null; path: string | null; change: string | null } {
  const params = new URLSearchParams(location.search);
  // `change` is what a refused push links to. hooks/update prints
  // `https://code.cresset.tools/?change=<id>` for every commit that has not been approved, so
  // the message names the work AND the place to do something about it.
  return {
    revision: params.get('revision'),
    path: params.get('path'),
    change: params.get('change'),
  };
}

function setViewUrl(revision: Revision, path: string | null, push: boolean) {
  const url = new URL(location.href);
  url.searchParams.set('revision', revision.commit_id);
  if (path == null) url.searchParams.delete('path');
  else url.searchParams.set('path', path);
  history[push ? 'pushState' : 'replaceState'](null, '', url);
}

function findRevision(revisions: Revision[], id: string): Revision | undefined {
  return revisions.find((revision) => revision.commit_id.startsWith(id) || revision.change_id.startsWith(id));
}

async function restoreUrlState() {
  const state = readUrlState();
  // Reads the LIVE loaded list: paging means what is loaded grows over time, and popstate can
  // fire long after the first page.
  let revision = state.revision == null
    ? loadedRevisions.find((candidate) => candidate.working_copy) ?? loadedRevisions[0]
    : findRevision(loadedRevisions, state.revision);
  if (revision == null && state.revision != null) {
    revision = (await fetchRevisionOrNull(state.revision)) ?? undefined;
    if (revision == null) {
      renderMissingRevision(state.revision);
      return;
    }
  }
  if (revision == null) return;
  currentMode = state.path == null ? defaultModeFor(revision) : 'changes';
  syncModeButtons();
  await selectRevision(revision, revisionButtons.get(revision.commit_id) ?? null, false);
}

function requiredElement<T extends HTMLElement = HTMLElement>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (element == null) throw new Error(`missing ${selector}`);
  return element;
}

function short(id: string): string {
  return id.slice(0, 12);
}

function firstLine(value: string): string {
  return value.split('\n', 1)[0] ?? '';
}

function escapeHtml(value: string): string {
  const node = document.createElement('span');
  node.textContent = value;
  return node.innerHTML;
}


// ---------------------------------------------------------------------------
// Synchronization status.
//
// cresset-sync pauses EVERY project when one conflicts, so a stall is easy to miss: nothing
// looks broken, work simply stops flowing. This banner is the standing reminder, and it appears
// only when there is something to say — a repository viewer should not carry a permanent green
// bar telling you nothing is wrong.
// ---------------------------------------------------------------------------

interface SyncProject {
  id: string;
  status: string;
  enabled: boolean;
  last_error?: string;
  conflict_operation_id?: string;
  conflict_commit?: string;
}

interface SyncResponse {
  available: boolean;
  unavailable_reason?: string;
  last_pass_age_secs?: number;
  projects: SyncProject[];
  incomplete_operations: number;
}

/** Beyond this, a fleet that should reconcile every five minutes has stopped. */
const STALE_AFTER_SECONDS = 1800;

async function renderSyncStatus(): Promise<void> {
  let sync: SyncResponse;
  try {
    sync = await fetchJson<SyncResponse>('/api/sync');
  } catch {
    // The worker is optional and this panel is secondary; never let it break the viewer.
    return;
  }
  if (!sync.available) return;

  const blocked = sync.projects.filter((project) => project.status === 'blocked');
  const stale =
    sync.last_pass_age_secs != null && sync.last_pass_age_secs > STALE_AFTER_SECONDS;
  if (blocked.length === 0 && !stale) return;

  const parts: string[] = [];
  if (blocked.length > 0) {
    parts.push(
      `<strong>${blocked.length === 1 ? '1 project is' : `${blocked.length} projects are`} blocked</strong>` +
        ' — synchronization is paused for every project until this is resolved.',
    );
    for (const project of blocked) {
      const link =
        project.conflict_commit != null
          ? `<a href="?revision=${encodeURIComponent(project.conflict_commit)}">${escapeHtml(project.id)}</a>`
          : escapeHtml(project.id);
      const resolve =
        project.conflict_operation_id != null
          ? ` <code>cresset-sync resolve ${escapeHtml(project.conflict_operation_id)} --jj-commit &lt;sha&gt;</code>`
          : '';
      parts.push(`<div class="sync-project">${link}${resolve}</div>`);
    }
  }
  if (stale) {
    const minutes = Math.floor((sync.last_pass_age_secs ?? 0) / 60);
    parts.push(
      `<div class="sync-project">No reconciliation pass has completed for ${minutes} minutes.</div>`,
    );
  }

  syncBanner.innerHTML = parts.join('');
  syncBanner.hidden = false;
}

/// Render a conflicted path.
///
/// This screen used to say "This path contains an unresolved jj conflict." and stop, which is
/// where the whole escalation path arrives: the worker pauses every project, Telegram carries a
/// pointer to exactly this URL, and the reader was then told only that the thing they had come
/// to look at existed. The point of showing the sides is that the decision — which version
/// wins, or what merge of them does — is the one thing a person is here to make.
function renderConflict(path: string, conflict: ConflictView | undefined) {
  const section = document.createElement('section');
  section.className = 'conflict-view';

  if (conflict == null) {
    // A conflict the backend could not decompose — a file against a directory, say. Rare, real,
    // and not a reason to fail: say what is known rather than pretending it is a normal file.
    section.innerHTML = `
      <h3>${escapeHtml(path)}</h3>
      <p class="conflict-note">This path is conflicted in a form with no side-by-side reading
      (for example a file on one side and a directory on the other). Inspect it with
      <code>jj</code>.</p>`;
    content.replaceChildren(section);
    return;
  }

  const terms = [
    ...conflict.bases.map((term) => ({ term, kind: 'base' as const })),
    ...conflict.sides.map((term) => ({ term, kind: 'side' as const })),
  ];
  const sideCount = conflict.sides.length;
  const baseCount = conflict.bases.length;

  section.innerHTML = `
    <h3>${escapeHtml(path)}</h3>
    <p class="conflict-note">
      Unresolved conflict: ${sideCount} competing version${sideCount === 1 ? '' : 's'}
      over ${baseCount} common ancestor${baseCount === 1 ? '' : 's'}.
      Synchronization is paused for every project until it is resolved.
    </p>
    ${conflict.materialized == null ? '' : `
      <pre class="conflict-markers">${escapeHtml(conflict.materialized)}</pre>`}
    <details class="conflict-terms-detail">
      <summary>Each version in full (${terms.length})</summary>
      <div class="conflict-terms">
        ${terms.map(({ term, kind }) => `
          <figure class="conflict-term ${kind}">
            <figcaption>
              <span class="conflict-kind">${kind === 'base' ? 'ancestor' : 'version'}</span>
              ${term.label == null ? '' : `<code>${escapeHtml(term.label)}</code>`}
            </figcaption>
            ${conflictTermBody(term)}
          </figure>`).join('')}
      </div>
    </details>
  `;
  content.replaceChildren(section);
}

function conflictTermBody(term: ConflictTerm): string {
  // An absent term is a DELETE on that side, and reading it as "empty file" would hide the
  // actual disagreement — one side removed the path, the other changed it.
  if (term.absent) return '<p class="conflict-empty">does not exist on this side</p>';
  if (term.binary) return '<p class="conflict-empty">binary</p>';
  if (term.contents == null) return '<p class="conflict-empty">too large to display</p>';
  return `<pre>${escapeHtml(term.contents)}</pre>`;
}

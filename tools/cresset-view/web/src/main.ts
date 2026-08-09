import {
  CodeView,
  File as FileViewer,
  parseDiffFromFile,
  type CodeViewItem,
  type CodeViewOptions,
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

type ViewMode = 'browse' | 'changes';
type ThemePreference = 'auto' | 'light' | 'dark';

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
      <h2><span>Revisions</span><strong id="head-count"></strong></h2>
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
let codeView: CodeView | null = null;
let diffItems: CodeViewItem[] = [];
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

  const first = await loadRevisionPage(true);
  if (first == null) return;
  operation.textContent = `operation ${short(first.operation_id)}`;
  headCount.textContent = `${first.head_count.toLocaleString()} heads`;

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

async function setMode(mode: ViewMode) {
  if (mode === currentMode) return;
  currentMode = mode;
  syncModeButtons();
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

function codeViewPlaceholder(path: string): CodeViewItem {
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

function codeViewItem(path: string, fileDiff: FileDiffMetadata, version: number): CodeViewItem {
  return {
    id: path,
    type: 'diff',
    fileDiff,
    version,
    collapsed: false,
  };
}

function codeViewOptions(): CodeViewOptions<undefined> {
  return {
    theme: { dark: 'pierre-dark', light: 'pierre-light' },
    themeType: pierreThemeType(),
    diffStyle: 'unified',
    overflow: 'wrap',
    stickyHeaders: true,
    layout: { paddingTop: 22, paddingBottom: 22, gap: 18 },
  };
}

function cleanContentRenderers() {
  renderedFile?.cleanUp();
  renderedFile = null;
  codeView?.cleanUp();
  codeView = null;
  diffItems = [];
  loadedDiffPaths.clear();
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

function readUrlState(): { revision: string | null; path: string | null } {
  const params = new URLSearchParams(location.search);
  return { revision: params.get('revision'), path: params.get('path') };
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

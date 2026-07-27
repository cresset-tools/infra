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
import './style.css';

interface Revision {
  change_id: string;
  commit_id: string;
  parent_commit_ids: string[];
  description: string;
  author_name: string;
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
}

type ViewMode = 'browse' | 'changes';
type ThemePreference = 'auto' | 'light' | 'dark';

interface GraphSegment {
  fromLane: number;
  toLane: number;
  fromY: number;
  toY: number;
  colorLane: number;
}

interface GraphRow {
  lane: number;
  laneCount: number;
  segments: GraphSegment[];
}

const app = document.querySelector<HTMLElement>('#app');
if (app == null) throw new Error('missing app element');

app.innerHTML = `
  <header>
    <div>
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
  <section class="workspace">
    <aside class="changes">
      <h2><span>Revisions</span><strong id="head-count"></strong></h2>
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

const operation = requiredElement('#operation');
const revisionList = requiredElement('#revision-list');
const headCount = requiredElement('#head-count');
const revisionPan = requiredElement<HTMLInputElement>('#revision-pan');
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

async function initialize() {
  const response = await fetchJson<RevisionsResponse>('/api/revisions?limit=150');
  operation.textContent = `operation ${short(response.operation_id)}`;
  headCount.textContent = `${response.head_count.toLocaleString()} heads`;
  const revisionButtons = new Map<string, HTMLButtonElement>();
  const graphRows = layoutRevisionGraph(response.revisions);
  const maxGraphLanes = Math.max(1, ...graphRows.map((row) => row.laneCount));
  const graphLaneGap = 14;
  const graphWidth = Math.ceil(24 + (maxGraphLanes - 1) * graphLaneGap);

  for (const [index, revision] of response.revisions.entries()) {
    const button = document.createElement('button');
    button.className = 'revision';
    button.style.setProperty('--graph-width', `${graphWidth}px`);
    button.innerHTML = `
      ${renderRevisionGraph(graphRows[index], revision, graphWidth, graphLaneGap)}
      <span class="revision-copy">
        <span class="revision-id">${revision.working_copy ? '@ · ' : ''}${escapeHtml(short(revision.change_id))}</span>
        <strong>${escapeHtml(firstLine(revision.description) || '(no description)')}</strong>
        <small>${escapeHtml(revision.author_name)} · ${formatDate(revision.authored_at)}</small>
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
    revisionList.append(button);
  }
  syncRevisionPanRange();
  revisionPan.addEventListener('input', () => {
    revisionList.scrollLeft = Number(revisionPan.value);
  });
  revisionList.addEventListener('scroll', () => {
    revisionPan.value = String(revisionList.scrollLeft);
  }, { passive: true });
  new ResizeObserver(syncRevisionPanRange).observe(revisionList);

  let initialRevision = initialUrlState.revision == null
    ? response.revisions.find((revision) => revision.working_copy) ?? response.revisions[0]
    : findRevision(response.revisions, initialUrlState.revision);
  if (initialRevision == null && initialUrlState.revision != null) {
    initialRevision = await fetchJson<Revision>(`/api/revisions/${encodeURIComponent(initialUrlState.revision)}`);
  }
  if (initialRevision != null) {
    await selectRevision(initialRevision, revisionButtons.get(initialRevision.commit_id) ?? null, false);
  }
  window.addEventListener('popstate', () => void restoreUrlState(response.revisions, revisionButtons));
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

function syncRevisionPanRange() {
  const maximum = Math.max(0, revisionList.scrollWidth - revisionList.clientWidth);
  revisionPan.max = String(maximum);
  revisionPan.disabled = maximum === 0;
  revisionPan.value = String(Math.min(maximum, revisionList.scrollLeft));
}

async function selectRevision(revision: Revision, button: HTMLButtonElement | null, updateHistory: boolean) {
  currentRevision = revision;
  currentRevisionButton?.classList.remove('selected');
  currentRevisionButton = button;
  button?.classList.add('selected');
  if (updateHistory) setViewUrl(revision, null, true);
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
    content.innerHTML = '<p class="empty-state">Select a file to view its contents.</p>';
    const result = await fetchJson<TreeResponse>(`/api/revisions/${revision.commit_id}/tree`);
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
  const result = await fetchJson<FileResponse>(
    `/api/revisions/${revision.commit_id}/file?path=${encodeURIComponent(path)}`,
  );
  if (generation !== fileGeneration || revision.commit_id !== currentRevision?.commit_id || currentMode !== 'browse') return;
  operation.textContent = `operation ${short(result.operation_id)}`;
  content.replaceChildren();

  if (result.binary) {
    content.innerHTML = `<section class="file-message"><h3>${escapeHtml(path)}</h3><p>Binary or oversized content is not rendered.</p></section>`;
    return;
  }
  if (result.conflicted) {
    content.innerHTML = `<section class="file-message"><h3>${escapeHtml(path)}</h3><p>This path contains an unresolved jj conflict.</p></section>`;
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

function layoutRevisionGraph(revisions: Revision[]): GraphRow[] {
  const lanes: Array<string | null> = [];
  const rows: GraphRow[] = [];

  for (const revision of revisions) {
    let lane = lanes.indexOf(revision.commit_id);
    const continuesFromAbove = lane >= 0;
    if (lane < 0) {
      lane = lanes.indexOf(null);
      if (lane < 0) lane = lanes.length;
      lanes[lane] = revision.commit_id;
    }

    const segments: GraphSegment[] = [];
    for (let activeLane = 0; activeLane < lanes.length; activeLane += 1) {
      if (activeLane !== lane && lanes[activeLane] != null) {
        segments.push({ fromLane: activeLane, toLane: activeLane, fromY: 0, toY: 100, colorLane: activeLane });
      }
    }
    if (continuesFromAbove) {
      segments.push({ fromLane: lane, toLane: lane, fromY: 0, toY: 50, colorLane: lane });
    }

    lanes[lane] = null;
    for (const [parentIndex, parentId] of revision.parent_commit_ids.entries()) {
      let parentLane = lanes.indexOf(parentId);
      if (parentLane < 0) {
        parentLane = parentIndex === 0 && lanes[lane] == null ? lane : lanes.indexOf(null);
        if (parentLane < 0) parentLane = lanes.length;
        lanes[parentLane] = parentId;
      }
      segments.push({ fromLane: lane, toLane: parentLane, fromY: 50, toY: 100, colorLane: parentLane });
    }

    while (lanes.length > 0 && lanes.at(-1) == null) lanes.pop();
    rows.push({ lane, laneCount: Math.max(lane + 1, lanes.length), segments });
  }

  return rows;
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
      <svg viewBox="0 0 ${width} 100" preserveAspectRatio="none">${paths}</svg>
      <span class="${nodeClasses}" style="left: ${graphLaneX(row.lane, laneGap)}px"></span>
      ${revision.is_head ? `<span class="graph-head-label lane-${row.lane % 8}" style="left: ${graphLaneX(row.lane, laneGap) + 10}px">head</span>` : ''}
    </span>
  `;
}

function graphLaneX(lane: number, laneGap: number): number {
  return 12 + lane * laneGap;
}

function pierreThemeType(): ThemeTypes {
  return themePreference === 'auto' ? 'system' : themePreference;
}

async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  if (!response.ok) throw new Error(await response.text());
  return response.json() as Promise<T>;
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

async function restoreUrlState(
  revisions: Revision[],
  revisionButtons: Map<string, HTMLButtonElement>,
) {
  const state = readUrlState();
  let revision = state.revision == null
    ? revisions.find((candidate) => candidate.working_copy) ?? revisions[0]
    : findRevision(revisions, state.revision);
  if (revision == null && state.revision != null) {
    revision = await fetchJson<Revision>(`/api/revisions/${encodeURIComponent(state.revision)}`);
  }
  if (revision == null) return;
  currentMode = state.path == null ? 'browse' : 'changes';
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

function formatDate(value: string): string {
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(new Date(value));
}

function escapeHtml(value: string): string {
  const node = document.createElement('span');
  node.textContent = value;
  return node.innerHTML;
}

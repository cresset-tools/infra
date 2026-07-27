import { File as FileViewer, FileDiff, type ThemeTypes } from '@pierre/diffs';
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
  bookmarks: string[];
}

interface RevisionsResponse {
  operation_id: string;
  revisions: Revision[];
}

interface FileChange {
  path: string;
  before: string | null;
  after: string | null;
  conflicted: boolean;
  binary: boolean;
}

interface DiffResponse {
  operation_id: string;
  change_id: string;
  commit_id: string;
  files: FileChange[];
}

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
      <h2>Revisions</h2>
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
const fileTreeContainer = requiredElement('#file-tree');
const fileHeading = requiredElement('#file-heading');
const changeHeading = requiredElement('#change-heading');
const detail = requiredElement('.detail');
const content = requiredElement('#content');
const themeSelect = requiredElement<HTMLSelectElement>('#theme');
const modeButtons = [...document.querySelectorAll<HTMLButtonElement>('[data-mode]')];
let tree: FileTree | null = null;
let renderedFile: FileViewer | null = null;
let renderedDiffs: FileDiff[] = [];
let currentRevision: Revision | null = null;
let currentRevisionButton: HTMLButtonElement | null = null;
let currentMode: ViewMode = 'browse';
let selectionGeneration = 0;
let fileGeneration = 0;
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

const response = await fetchJson<RevisionsResponse>('/api/revisions?limit=150');
operation.textContent = `operation ${short(response.operation_id)}`;
const revisionButtons = new Map<string, HTMLButtonElement>();

for (const revision of response.revisions) {
  const button = document.createElement('button');
  button.className = 'revision';
  button.innerHTML = `
    <span class="revision-id">${revision.working_copy ? '@ · ' : ''}${escapeHtml(short(revision.change_id))}</span>
    <strong>${escapeHtml(firstLine(revision.description) || '(no description)')}</strong>
    <small>${escapeHtml(revision.author_name)} · ${formatDate(revision.authored_at)}</small>
    <span class="signals">
      ${revision.bookmarks.map((name) => `<em>${escapeHtml(name)}</em>`).join('')}
      ${revision.working_copy ? '<em>@</em>' : ''}
      ${revision.divergent ? '<em class="warning">divergent</em>' : ''}
      ${revision.has_conflict ? '<em class="warning">conflict</em>' : ''}
    </span>
  `;
  button.addEventListener('click', () => void selectRevision(revision, button));
  revisionButtons.set(revision.commit_id, button);
  revisionList.append(button);
}

const initialRevision = response.revisions.find((revision) => revision.working_copy) ?? response.revisions[0];
if (initialRevision != null) {
  await selectRevision(initialRevision, revisionButtons.get(initialRevision.commit_id)!);
}

async function setMode(mode: ViewMode) {
  if (mode === currentMode) return;
  currentMode = mode;
  syncModeButtons();
  if (currentRevision != null) await loadRevision(currentRevision);
}

function syncModeButtons() {
  for (const button of modeButtons) {
    const selected = button.dataset.mode === currentMode;
    button.classList.toggle('selected', selected);
    button.setAttribute('aria-pressed', String(selected));
  }
}

async function selectRevision(revision: Revision, button: HTMLButtonElement) {
  currentRevision = revision;
  currentRevisionButton?.classList.remove('selected');
  currentRevisionButton = button;
  button.classList.add('selected');
  await loadRevision(revision);
}

async function loadRevision(revision: Revision) {
  const generation = ++selectionGeneration;
  fileGeneration += 1;
  cleanContentRenderers();
  detail.scrollTop = 0;
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
  const result = await fetchJson<DiffResponse>(`/api/revisions/${revision.commit_id}/diff`);
  if (generation !== selectionGeneration) return;
  const preparedInput = prepareFileTreeInput(result.files.map((file) => file.path), {
    flattenEmptyDirectories: false,
  });
  const filesByPath = new Map(result.files.map((file) => [file.path, file]));
  const sortedFiles = preparedInput.paths.map((path) => filesByPath.get(path)!);
  operation.textContent = `operation ${short(result.operation_id)}`;
  fileHeading.textContent = `${result.files.length.toLocaleString()} changed files`;
  renderFileTree(preparedInput, 'open', scrollToDiff);
  renderDiffs(sortedFiles);
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

function scrollToDiff(path: string) {
  const target = document.querySelector<HTMLElement>(`[data-diff-path="${CSS.escape(path)}"]`);
  if (target == null) return;
  const top = target.getBoundingClientRect().top - detail.getBoundingClientRect().top
    + detail.scrollTop - changeHeading.offsetHeight - 12;
  detail.scrollTo({ top, behavior: 'smooth' });
}

function renderDiffs(files: FileChange[]) {
  cleanContentRenderers();
  content.replaceChildren();

  if (files.length === 0) {
    content.innerHTML = '<p class="empty-state">This revision has no file changes.</p>';
    return;
  }

  for (const file of files) {
    const section = document.createElement('section');
    section.className = 'file-diff';
    section.dataset.diffPath = file.path;
    if (file.binary) {
      section.innerHTML = `<h3>${escapeHtml(file.path)}</h3><p>Binary or oversized content is not rendered.</p>`;
    } else if (file.conflicted) {
      section.innerHTML = `<h3>${escapeHtml(file.path)}</h3><p>This path contains an unresolved jj conflict.</p>`;
    } else {
      const mount = document.createElement('div');
      section.append(mount);
      const instance = new FileDiff({
        theme: { dark: 'pierre-dark', light: 'pierre-light' },
        themeType: pierreThemeType(),
        diffStyle: 'unified',
        overflow: 'wrap',
      });
      instance.render({
        oldFile: { name: file.path, contents: file.before ?? '' },
        newFile: { name: file.path, contents: file.after ?? '' },
        containerWrapper: mount,
      });
      renderedDiffs.push(instance);
    }
    content.append(section);
  }
}

function cleanContentRenderers() {
  renderedFile?.cleanUp();
  renderedFile = null;
  for (const instance of renderedDiffs) instance.cleanUp();
  renderedDiffs = [];
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
  for (const instance of renderedDiffs) instance.setThemeType(themeType);
}

function applyTreeTheme() {
  const colorScheme = themePreference === 'auto' ? 'light dark' : themePreference;
  tree?.getFileTreeContainer()?.style.setProperty('color-scheme', colorScheme);
}

function pierreThemeType(): ThemeTypes {
  return themePreference === 'auto' ? 'system' : themePreference;
}

async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  if (!response.ok) throw new Error(await response.text());
  return response.json() as Promise<T>;
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

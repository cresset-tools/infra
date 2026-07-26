import { FileDiff } from '@pierre/diffs';
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

const app = document.querySelector<HTMLElement>('#app');
if (app == null) throw new Error('missing app element');

app.innerHTML = `
  <header>
    <div>
      <span class="eyebrow">Cresset internal</span>
      <h1>Code</h1>
    </div>
    <code id="operation">loading operation</code>
  </header>
  <section class="workspace">
    <aside class="changes">
      <h2>Revisions</h2>
      <div id="revision-list" class="revision-list"></div>
    </aside>
    <aside class="files">
      <h2 id="file-heading">Changed files</h2>
      <div id="file-tree"></div>
    </aside>
    <article class="detail">
      <div id="change-heading" class="change-heading">
        <p>Select a change to inspect its exact commit version.</p>
      </div>
      <div id="diffs"></div>
    </article>
  </section>
`;

const operation = requiredElement('#operation');
const revisionList = requiredElement('#revision-list');
const fileTreeContainer = requiredElement('#file-tree');
const fileHeading = requiredElement('#file-heading');
const changeHeading = requiredElement('#change-heading');
const detail = requiredElement('.detail');
const diffs = requiredElement('#diffs');
let tree: FileTree | null = null;
let renderedDiffs: FileDiff[] = [];
let selectionGeneration = 0;

const response = await fetchJson<RevisionsResponse>('/api/revisions?limit=150');
operation.textContent = `operation ${short(response.operation_id)}`;

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
  revisionList.append(button);
}

async function selectRevision(revision: Revision, button: HTMLButtonElement) {
  const generation = ++selectionGeneration;
  document.querySelectorAll('.revision.selected').forEach((node) => node.classList.remove('selected'));
  button.classList.add('selected');
  changeHeading.innerHTML = `
    <div><code>${escapeHtml(short(revision.change_id))}</code><span>change</span></div>
    <h2>${escapeHtml(firstLine(revision.description) || '(no description)')}</h2>
    <p>commit <code>${escapeHtml(short(revision.commit_id))}</code> by ${escapeHtml(revision.author_name)}</p>
  `;
  diffs.textContent = 'Loading comparison…';
  fileHeading.textContent = 'Loading changed files…';

  const result = await fetchJson<DiffResponse>(`/api/revisions/${revision.commit_id}/diff`);
  if (generation !== selectionGeneration) return;

  const preparedInput = prepareFileTreeInput(result.files.map((file) => file.path), {
    flattenEmptyDirectories: false,
  });
  const filesByPath = new Map(result.files.map((file) => [file.path, file]));
  const sortedFiles = preparedInput.paths.map((path) => filesByPath.get(path)!);
  operation.textContent = `operation ${short(result.operation_id)}`;
  fileHeading.textContent = `${result.files.length.toLocaleString()} changed files`;
  renderFileTree(preparedInput);
  renderDiffs(sortedFiles);
}

function renderFileTree(preparedInput: FileTreePreparedInput) {
  tree?.cleanUp();
  fileTreeContainer.replaceChildren();
  tree = new FileTree({
    preparedInput,
    initialExpansion: 'open',
    initialVisibleRowCount: 40,
    search: true,
    density: 'compact',
    onSelectionChange(paths) {
      const path = paths[0];
      if (path == null) return;
      const target = document.querySelector<HTMLElement>(`[data-diff-path="${CSS.escape(path)}"]`);
      if (target == null) return;
      const top = target.getBoundingClientRect().top - detail.getBoundingClientRect().top
        + detail.scrollTop - changeHeading.offsetHeight - 12;
      detail.scrollTo({ top, behavior: 'smooth' });
    },
  });
  tree.render({ containerWrapper: fileTreeContainer });
}

function renderDiffs(files: FileChange[]) {
  for (const instance of renderedDiffs) instance.cleanUp();
  renderedDiffs = [];
  diffs.replaceChildren();

  if (files.length === 0) {
    diffs.textContent = 'This commit has no file changes.';
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
    diffs.append(section);
  }
}

async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  if (!response.ok) throw new Error(await response.text());
  return response.json() as Promise<T>;
}

function requiredElement(selector: string): HTMLElement {
  const element = document.querySelector<HTMLElement>(selector);
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

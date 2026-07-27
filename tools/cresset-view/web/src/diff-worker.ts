import { parseDiffFromFile } from '../node_modules/@pierre/diffs/dist/utils/parseDiffFromFile.js';

interface DiffWorkerFile {
  path: string;
  before: string | null;
  after: string | null;
  conflicted: boolean;
  binary: boolean;
}

interface DiffWorkerRequest {
  id: number;
  file: DiffWorkerFile;
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
    self.postMessage({ id, fileDiff });
  } catch (error) {
    self.postMessage({ id, error: error instanceof Error ? error.message : String(error) });
  }
});

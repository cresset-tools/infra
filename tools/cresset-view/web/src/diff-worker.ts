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
    // A conflicted file is diffed like any other.
    //
    // This used to substitute 'This path contains an unresolved jj conflict.' for the content
    // and diff THAT, so the Changes view discarded whatever the server sent and rendered the
    // same dead end the file view used to. Both sides now arrive as jj's marker text when
    // unresolved, so the ordinary diff shows the parent's content against the conflict — which
    // is what someone opening Changes came to see.
    //
    // Binary keeps its notice: there is genuinely nothing to diff, and the server says so by
    // sending no text. That includes a conflict with no textual form at all, such as a file
    // against a directory.
    const notice = file.binary ? 'Binary or oversized content is not rendered.\n' : null;
    const fileDiff = parseDiffFromFile(
      { name: file.path, contents: notice == null ? file.before ?? '' : '' },
      { name: file.path, contents: notice ?? file.after ?? '' },
    );
    self.postMessage({ id, fileDiff });
  } catch (error) {
    self.postMessage({ id, error: error instanceof Error ? error.message : String(error) });
  }
});

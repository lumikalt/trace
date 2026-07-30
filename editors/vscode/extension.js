// Plain CommonJS, no build step: shells out to `trace - --fmt` (reads
// the document text on stdin, writes formatted text to stdout — see
// src/main.rs / src/fmt.rs) and replaces the whole document with the
// result. Piping the in-editor buffer rather than reading the file
// from disk means unsaved edits get formatted correctly, not stale
// on-disk content.
const vscode = require('vscode');
const { spawn } = require('child_process');

function formatWithTrace(text, bin) {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, ['-', '--fmt']);
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => (stdout += chunk));
    child.stderr.on('data', (chunk) => (stderr += chunk));
    child.on('error', (err) => reject(new Error(`failed to run "${bin}": ${err.message}`)));
    child.on('close', (code) => {
      if (code !== 0) {
        reject(new Error(`exit ${code}: ${stderr.trim() || '(no output)'}`));
      } else {
        resolve(stdout);
      }
    });
    child.stdin.write(text);
    child.stdin.end();
  });
}

function activate(context) {
  const provider = vscode.languages.registerDocumentFormattingEditProvider('trace', {
    async provideDocumentFormattingEdits(document) {
      const bin = vscode.workspace.getConfiguration('trace').get('formatterPath') || 'trace';
      try {
        const formatted = await formatWithTrace(document.getText(), bin);
        const fullRange = new vscode.Range(
          document.positionAt(0),
          document.positionAt(document.getText().length)
        );
        return [vscode.TextEdit.replace(fullRange, formatted)];
      } catch (err) {
        vscode.window.showErrorMessage(`trace fmt: ${err.message}`);
        return [];
      }
    },
  });
  context.subscriptions.push(provider);
}

function deactivate() {}

module.exports = { activate, deactivate };

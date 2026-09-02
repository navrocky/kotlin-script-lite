import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const serverPath = resolveServerPath();
  if (!serverPath) {
    vscode.window.showErrorMessage(
      "ksl-lsp binary not found. Build it with 'cargo build -p ksl-lsp' in the " +
        "kotlin-script-lite workspace, or set the 'ksl.server.path' setting to " +
        "point at a built binary."
    );
    return;
  }

  const serverOptions: ServerOptions = {
    command: serverPath,
    args: [],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "ksl" }],
  };

  client = new LanguageClient(
    "ksl-lsp",
    "KSL Language Server",
    serverOptions,
    clientOptions
  );
  client.start();
  context.subscriptions.push({ dispose: () => client?.stop() });
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

/**
 * Resolves the ksl-lsp binary to run:
 * 1. The `ksl.server.path` setting, if set and it exists.
 * 2. `target/{release,debug}/ksl-lsp` under each open workspace folder —
 *    this repo's own dev loop (editing KSL itself against a freshly built
 *    server).
 *
 * Packaging a per-platform binary with the extension for end users editing
 * arbitrary .ksl files elsewhere is deliberately out of scope here — see
 * the "explicitly deferred" section of the LSP support plan.
 */
function resolveServerPath(): string | undefined {
  const configured = vscode.workspace
    .getConfiguration("ksl")
    .get<string>("server.path");
  if (configured && configured.trim().length > 0 && fs.existsSync(configured)) {
    return configured;
  }

  const binaryName = process.platform === "win32" ? "ksl-lsp.exe" : "ksl-lsp";
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    for (const profile of ["release", "debug"]) {
      const candidate = path.join(folder.uri.fsPath, "target", profile, binaryName);
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}

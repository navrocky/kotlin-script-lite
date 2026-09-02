# KSL Language Support (VSCode extension, dev scaffold)

Diagnostics and syntax highlighting for `.ksl` files, backed by the
`ksl-lsp` language server from this repo. This is the extension's dev-mode
scaffold — there is no packaging/publishing pipeline yet (see the LSP
support plan's "explicitly deferred" section).

## Running it

1. Build the language server from the repo root:
   ```sh
   cargo build -p ksl-lsp
   ```
2. Install the extension's own dependencies:
   ```sh
   npm install
   ```
3. Open this `editors/vscode/` folder in VSCode and press `F5` (or use the
   "Run KSL Extension" launch config) to open an Extension Development Host
   with the extension loaded.
4. In that new window, open a folder containing `.ksl` files (e.g. this
   repo's own root, so `crates/ksl-core/tests/examples/*.ksl` are handy) and
   open one. The extension looks for `ksl-lsp` at
   `target/{release,debug}/ksl-lsp` under the opened workspace folder.

If the server binary is somewhere else, set the `ksl.server.path` setting
to its path instead.

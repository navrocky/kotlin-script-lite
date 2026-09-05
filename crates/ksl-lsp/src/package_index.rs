//! Thin `ksl-lsp`-side wrapper over `ksl_core::package_index` — the real
//! cross-file indexing logic lives there now (shared with
//! `resolver::resolve_with_imports`, which needs the same `PackageIndex` as
//! an input for `ksl-cli`/`run_source`'s own real `import` resolution). This
//! module only adds the `PathBuf` <-> `Url` conversion `tower_lsp`'s API
//! needs, since `ksl-core` can't depend on `tower_lsp`.

use std::path::Path;

pub use ksl_core::package_index::{build_package_index, PackageIndex, PackageMember};

use tower_lsp::lsp_types::Url;

pub(crate) fn path_to_url(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

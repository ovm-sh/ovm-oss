use crate::error::Result;
use crate::version_manager::{InstallRequest, VersionManager};

pub fn run(vm: &VersionManager, request: InstallRequest) -> Result<()> {
    let standard_install = matches!(request, InstallRequest::Standard { .. });
    vm.install(request)?;

    // An unmanaged copy earlier on PATH silently wins over what was just
    // installed, and only `launch`/`adopt` used to say so — someone who only
    // ever ran `ovm install` never saw the guidance. One advisory line, and
    // only for standard installs: the dev flows (`--dev`, imports) are
    // developer loops where the coexistence is deliberate.
    if standard_install {
        if let Some(foreign) = super::adopt::foreign_binary_on_path(&vm.dirs, vm.product()) {
            eprintln!(
                "  {} An unmanaged {} also exists at {} (untouched) — it may shadow the managed \
                 install; see `ovm adopt {}`",
                console::style("→").dim(),
                vm.product().display_name(),
                foreign.display(),
                vm.product().canonical_name(),
            );
        }
    }
    Ok(())
}

use crate::error::Result;
use crate::version_manager::{InstallRequest, VersionManager};

pub fn run(vm: &VersionManager, request: InstallRequest) -> Result<()> {
    vm.install(request)?;
    Ok(())
}

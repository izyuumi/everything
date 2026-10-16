use super::super::authority::SettingsCaller;
use super::super::contract::ModelSelection;
use super::{SettingsService, SettingsServiceError, updates};

impl SettingsService {
    pub(crate) fn owner_grant_model(
        &self,
        caller: &SettingsCaller,
    ) -> Result<(u64, Option<ModelSelection>), SettingsServiceError> {
        self.core.authority.validate_owner_cleanup(caller)?;
        let settings = self.core.settings.observed_snapshot()?;
        Ok((settings.revision, updates::model_selection(&settings)))
    }
}

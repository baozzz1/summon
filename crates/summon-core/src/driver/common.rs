use crate::driver::{LaunchModel, ModelEffortError};
use std::time::Duration;

/// Other:不支持 model(supports_model=None)也不支持 effort。传 effort → `EffortUnsupported`;
/// 否则空片段(连 model 都不下发,维持现状)。
pub fn resolve_launch(
    _model: Option<&str>,
    effort: Option<&str>,
) -> Result<LaunchModel, ModelEffortError> {
    if effort.is_some() {
        return Err(ModelEffortError::EffortUnsupported { cli: "other" });
    }
    Ok(LaunchModel::default())
}

pub fn default_cmd() -> &'static str {
    "claude"
}

pub fn captures() -> bool {
    false
}

pub fn supports_model() -> Option<&'static str> {
    None
}

pub fn system_prompt_file_flag() -> Option<&'static str> {
    None
}

pub fn clear_input_before_send() -> bool {
    false
}

pub fn injects_by_paste() -> bool {
    false
}

pub fn interrupt_key() -> &'static str {
    "Escape"
}

pub fn interrupt_grace() -> Duration {
    Duration::from_millis(500)
}

pub fn startup_grace() -> Duration {
    Duration::from_millis(1500)
}

use crate::ui::state::View;
use yourai_core::prelude::YourAiError;
use yourai_harness::Harness;

pub(in crate::ui) async fn restore_history(h: &Harness, v: &mut View) -> Result<(), YourAiError> {
    super::apply_history(yourai_harness::controller::history(h).await?, v);
    Ok(())
}

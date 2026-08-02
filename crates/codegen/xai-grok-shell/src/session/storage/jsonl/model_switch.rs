use super::*;
use crate::session::storage::ModelSwitchCommitError;

const MODEL_SWITCH_JOURNAL_FILE: &str = "model_switch.intent.json";
const MODEL_SWITCH_LOCK_FILE: &str = "model_switch.intent.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSwitchCommitStep {
    IntentDurable,
    ChatDurable,
    SummaryDurable,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ModelSwitchIntent {
    version: u8,
    messages: Vec<ConversationItem>,
    model_id: acp::ModelId,
    agent_name: Option<String>,
    reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
}

impl JsonlStorageAdapter {
    pub(super) fn model_switch_journal_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(MODEL_SWITCH_JOURNAL_FILE)
    }

    pub(super) fn commit_model_switch_sync(
        &self,
        info: &Info,
        messages: &[ConversationItem],
        model_id: &acp::ModelId,
        agent_name: Option<&str>,
        reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
    ) -> Result<(), ModelSwitchCommitError> {
        let session_dir = self.session_dir(info);
        let lock = self
            .open_model_switch_lock(&session_dir)
            .map_err(ModelSwitchCommitError::NotCommitted)?;
        lock.lock_exclusive()
            .map_err(ModelSwitchCommitError::NotCommitted)?;
        if let Err(error) = self.recover_model_switch_locked(&session_dir) {
            let _ = lock.unlock();
            return Err(ModelSwitchCommitError::NotCommitted(error));
        }
        let intent = ModelSwitchIntent {
            version: 1,
            messages: messages.to_vec(),
            model_id: model_id.clone(),
            agent_name: agent_name.map(str::to_owned),
            reasoning_effort,
        };
        let journal = session_dir.join(MODEL_SWITCH_JOURNAL_FILE);
        let bytes = serde_json::to_vec_pretty(&intent).map_err(|error| {
            ModelSwitchCommitError::NotCommitted(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;
        if let Err(error) = super::super::write_bytes_atomic_durable(&journal, &bytes) {
            let committed = journal.exists();
            let _ = lock.unlock();
            return Err(if committed {
                ModelSwitchCommitError::Committed(error)
            } else {
                ModelSwitchCommitError::NotCommitted(error)
            });
        }
        if let Err(error) = self.probe_model_switch(ModelSwitchCommitStep::IntentDurable) {
            let _ = lock.unlock();
            return Err(ModelSwitchCommitError::Committed(error));
        }
        let result = self.materialize_model_switch_intent(&session_dir, &intent);
        let _ = lock.unlock();
        result.map_err(ModelSwitchCommitError::Committed)
    }

    pub(super) fn recover_model_switch_sync(&self, info: &Info) -> io::Result<()> {
        self.recover_model_switch_in_dir_sync(&self.session_dir(info))
    }

    pub(super) fn recover_model_switch_in_dir_sync(&self, session_dir: &Path) -> io::Result<()> {
        let lock = self.open_model_switch_lock(session_dir)?;
        lock.lock_exclusive()?;
        let result = self.recover_model_switch_locked(session_dir);
        let _ = lock.unlock();
        result
    }

    fn recover_model_switch_locked(&self, session_dir: &Path) -> io::Result<()> {
        let journal = session_dir.join(MODEL_SWITCH_JOURNAL_FILE);
        let bytes = match std::fs::read(&journal) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let intent: ModelSwitchIntent = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if intent.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported model-switch intent version {}", intent.version),
            ));
        }
        self.materialize_model_switch_intent(session_dir, &intent)
    }

    fn materialize_model_switch_intent(
        &self,
        session_dir: &Path,
        intent: &ModelSwitchIntent,
    ) -> io::Result<()> {
        let chat_bytes = super::super::to_jsonl_bytes(&intent.messages)?;
        super::super::write_bytes_atomic_durable(
            &session_dir.join(super::super::CHAT_HISTORY_FILE),
            &chat_bytes,
        )?;
        self.probe_model_switch(ModelSwitchCommitStep::ChatDurable)?;

        let cwd_switch_bookkeeping_generation = intent
            .messages
            .iter()
            .filter_map(ConversationItem::working_directory_switch_generation)
            .max()
            .unwrap_or(0);
        super::super::summary_write::apply_patch_locked_durable(
            &session_dir.join(super::super::SUMMARY_FILE),
            &session_dir.join(format!("{}.lock", super::super::SUMMARY_FILE)),
            &super::super::summary_write::SummaryPatch {
                chat_messages: Some(super::super::summary_write::CounterOp::Set(
                    intent.messages.len(),
                )),
                chat_format_version: Some(CHAT_FORMAT_VERSION),
                cwd_switch_bookkeeping_generation: Some(cwd_switch_bookkeeping_generation),
                model: Some(super::super::summary_write::ModelPatch {
                    model_id: intent.model_id.clone(),
                    agent_name: intent.agent_name.clone(),
                    reasoning_effort: Some(intent.reasoning_effort),
                }),
                ..Default::default()
            },
        )?;
        self.probe_model_switch(ModelSwitchCommitStep::SummaryDurable)?;

        let journal = session_dir.join(MODEL_SWITCH_JOURNAL_FILE);
        std::fs::remove_file(&journal)?;
        super::super::sync_parent_directory(&journal)
    }

    fn open_model_switch_lock(&self, session_dir: &Path) -> io::Result<std::fs::File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(session_dir.join(MODEL_SWITCH_LOCK_FILE))
    }

    #[cfg(test)]
    fn probe_model_switch(&self, step: ModelSwitchCommitStep) -> io::Result<()> {
        match &self.model_switch_probe {
            Some(probe) => probe(step),
            None => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn probe_model_switch(&self, _step: ModelSwitchCommitStep) -> io::Result<()> {
        Ok(())
    }
}

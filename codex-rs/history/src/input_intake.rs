// Modified by RenCrow Switch Core, 2026-09-22.
//! Intake facts supplied by the input boundary, never inferred from model history.
use crate::compaction_candidate::digest;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAuthor {
    Human,
    Automation,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalInput {
    pub author: InputAuthor,
    pub text: String,
    pub attachments: Vec<Value>,
}

impl std::fmt::Debug for OriginalInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalInput")
            .field("author", &self.author)
            .finish_non_exhaustive()
    }
}

/// A submission is not acceptance. A collector must also match the host's
/// user-message event, owning thread and exact submitted text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionIntake {
    pub version: u32,
    pub thread_id: String,
    pub client_id: String,
    pub original: OriginalInput,
    pub submitted_text_hash: String,
}

pub fn valid_intake_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl SubmissionIntake {
    pub fn new(
        thread_id: String,
        client_id: String,
        original: OriginalInput,
        submitted_text: &str,
    ) -> Result<Self, String> {
        if !valid_intake_id(&thread_id) || !valid_intake_id(&client_id) {
            return Err("invalid intake identity".into());
        }
        if !submitted_text.starts_with(&original.text) {
            return Err("original input was rewritten before submission".into());
        }
        Ok(Self {
            version: 1,
            thread_id,
            client_id,
            original,
            submitted_text_hash: digest(&submitted_text)?,
        })
    }

    pub fn matches_accepted(&self, thread_id: &str, client_id: &str, submitted_text: &str) -> bool {
        self.version == 1
            && valid_intake_id(&self.thread_id)
            && valid_intake_id(&self.client_id)
            && self.thread_id == thread_id
            && self.client_id == client_id
            && submitted_text.starts_with(&self.original.text)
            && digest(&submitted_text).is_ok_and(|hash| hash == self.submitted_text_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_requires_exact_thread_client_and_accepted_text() {
        let original = OriginalInput {
            author: InputAuthor::Human,
            text: "本人の本文。".into(),
            attachments: vec![serde_json::json!({"kind":"local_image","path":"添付.png"})],
        };
        let receipt = SubmissionIntake::new(
            "thread-1".into(),
            "client-1".into(),
            original.clone(),
            "本人の本文。IDE context",
        )
        .unwrap();
        assert!(receipt.matches_accepted("thread-1", "client-1", "本人の本文。IDE context"));
        assert!(!receipt.matches_accepted("thread-2", "client-1", "本人の本文。IDE context"));
        assert!(!receipt.matches_accepted("thread-1", "client-2", "本人の本文。IDE context"));
        assert!(!receipt.matches_accepted("thread-1", "client-1", "本人の本文。changed"));
        assert!(
            SubmissionIntake::new(
                "../escape".into(),
                "client-1".into(),
                original.clone(),
                "本人の本文。"
            )
            .is_err()
        );
        assert!(
            SubmissionIntake::new("thread-1".into(), "client-1".into(), original, "rewritten")
                .is_err()
        );
    }
}

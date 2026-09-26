// Modified by RenCrow Switch Core, 2026-09-22.
//! Composer-originated input is recorded separately before host context is added.
//! A record is only a submission: the reader must verify the server acceptance.
use crate::bottom_pane::LocalImageAttachment;
use codex_app_server_protocol::UserInput;
use codex_history::input_intake::InputAuthor;
use codex_history::input_intake::OriginalInput;
use codex_history::input_intake::SubmissionIntake;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

static AUTHOR: AtomicU8 = AtomicU8::new(0);

/// Shown when the TUI starts without declaring who types into it.
pub(crate) const MISSING_AUTHOR_MESSAGE: &str = "--rencrow-input-author is required: pass `--rencrow-input-author human` when a person types into this TUI, or `--rencrow-input-author automation` when another program operates it";

/// Refuse to start unless the operator of this input channel is declared.
pub(crate) fn require_author(author: Option<&str>) -> std::io::Result<()> {
    match author {
        Some("human" | "automation") => Ok(()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            MISSING_AUTHOR_MESSAGE,
        )),
    }
}

pub(crate) fn set_author(author: Option<&str>) {
    AUTHOR.store(
        match author {
            Some("human") => 1,
            Some("automation") => 2,
            _ => 0,
        },
        Ordering::Relaxed,
    );
}

pub(crate) fn original(
    text: &str,
    images: &[LocalImageAttachment],
    remote_images: &[String],
) -> Option<OriginalInput> {
    let author = match AUTHOR.load(Ordering::Relaxed) {
        1 => InputAuthor::Human,
        2 => InputAuthor::Automation,
        _ => return None,
    };
    let attachments = images
        .iter()
        .map(|image| {
            serde_json::json!({"kind":"local_image","path":image.path,"placeholder":image.placeholder})
        })
        .chain(remote_images.iter().map(|url| {
            serde_json::json!({"kind":"remote_image","url":url})
        }))
        .collect();
    Some(OriginalInput {
        author,
        text: text.into(),
        attachments,
    })
}

pub(crate) fn record(
    codex_home: &Path,
    thread_id: &str,
    client_id: &str,
    original: &OriginalInput,
    submitted: &[UserInput],
) -> Result<(), String> {
    let text = submitted
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let receipt =
        SubmissionIntake::new(thread_id.into(), client_id.into(), original.clone(), &text)?;
    let directory = codex_home
        .join("rencrow")
        .join("input-intake")
        .join(thread_id);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&directory)
        .map_err(|error| error.to_string())?;
    let data = serde_json::to_vec(&receipt).map_err(|error| error.to_string())?;
    if data.len() > 1_048_576 {
        return Err(
            "input-intake metadata exceeds the recording limit; provenance stays unknown".into(),
        );
    }
    let mut file =
        tempfile::NamedTempFile::new_in(&directory).map_err(|error| error.to_string())?;
    file.write_all(&data).map_err(|error| error.to_string())?;
    file.as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    file.persist_noclobber(directory.join(format!("{client_id}.json")))
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_original_text_and_attachments_without_host_context() {
        let home = tempfile::tempdir().unwrap();
        let original = OriginalInput {
            author: InputAuthor::Human,
            text: "本人本文".into(),
            attachments: vec![serde_json::json!({"kind":"local_image","path":"添付.png"})],
        };
        let submitted = vec![
            UserInput::Text {
                text: "本人本文".into(),
                text_elements: vec![],
            },
            UserInput::Text {
                text: "\nIDE context".into(),
                text_elements: vec![],
            },
        ];
        record(home.path(), "thread-1", "client-1", &original, &submitted).unwrap();
        let path = home
            .path()
            .join("rencrow/input-intake/thread-1/client-1.json");
        let bytes = std::fs::read(&path).unwrap();
        let receipt: SubmissionIntake = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt.original, original);
        assert!(receipt.matches_accepted("thread-1", "client-1", "本人本文\nIDE context"));
        assert!(record(home.path(), "thread-1", "client-1", &original, &submitted).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn startup_requires_a_declared_input_author() {
        assert!(require_author(Some("human")).is_ok());
        assert!(require_author(Some("automation")).is_ok());
        let error = require_author(None).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), MISSING_AUTHOR_MESSAGE);
    }
}

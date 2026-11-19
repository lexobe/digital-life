//! Only a fresh desktop observation crosses the model boundary. The journal stores
//! its JPEG once and represents the inline image in request records by an artifact URI.
use crate::{
    computer::FRAME_FILE,
    journal::{ActionStatus, Journal},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{fs, io::Read, path::PathBuf};
use survive_core::{MAX_FRAME_BYTES, is_desktop_jpeg};

pub(crate) struct Observation {
    pub path: PathBuf,
    turn: u64,
    index: usize,
    width: u64,
    height: u64,
}

impl Observation {
    pub fn pending(journal: &Journal, current: u64) -> Result<Option<Self>> {
        for &id in journal
            .ids
            .iter()
            .rev()
            .filter(|&&id| id < current)
            .take(32)
        {
            let dir = journal.turn_dir(id);
            if !dir.join("state.json").exists() {
                continue;
            }
            let turn = journal.load(id)?;
            for (index, action) in turn.actions.iter().enumerate().rev() {
                if action.name != "computer" || action.status != ActionStatus::Done {
                    continue;
                }
                let Some(result) = action.result.as_ref() else {
                    continue;
                };
                if result["ok"] == true && result["action"] == "screenshot" {
                    return Ok(Some(Self {
                        path: journal.action_dir(id, index).join(FRAME_FILE),
                        turn: id,
                        index,
                        width: result["width"]
                            .as_u64()
                            .context("missing screenshot width")?,
                        height: result["height"]
                            .as_u64()
                            .context("missing screenshot height")?,
                    }));
                }
            }
            // An intervening inference had its opportunity to consume earlier
            // images, even if its network outcome was uncertain.
            if dir.join("model-request.json").exists() {
                break;
            }
        }
        Ok(None)
    }

    pub fn message(&self, responses: bool) -> Result<Value> {
        let mut bytes = Vec::new();
        fs::File::open(&self.path)?
            .take(MAX_FRAME_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(is_desktop_jpeg(&bytes), "invalid screenshot artifact");
        let url = format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes));
        let description = format!(
            "Sandbox screenshot from iteration {}, action {}, {}x{} pixels. Coordinates start at top left.",
            self.turn, self.index, self.width, self.height
        );
        Ok(if responses {
            json!({"role":"user","content":[{"type":"input_text","text":description},{"type":"input_image","image_url":url}]})
        } else {
            json!({"role":"user","content":[{"type":"text","text":description},{"type":"image_url","image_url":{"url":url}}]})
        })
    }

    pub fn journal_request(&self, request: &Value) -> Value {
        let mut record = request.clone();
        let uri = json!(format!("artifact:{}", self.path.display()));
        if let Some(message) = record
            .get_mut("input")
            .and_then(Value::as_array_mut)
            .and_then(|v| v.last_mut())
        {
            message["content"][1]["image_url"] = uri;
        } else if let Some(message) = record
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .and_then(|v| v.last_mut())
        {
            message["content"][1]["image_url"]["url"] = uri;
        }
        record
    }
}

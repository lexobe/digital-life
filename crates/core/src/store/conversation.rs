use super::*;
use std::ops::Bound::{Excluded, Included};

enum Sender<'a> {
    Creator(&'a str),
    Organism { reply_to: u64 },
}

fn message_key(id: &str, seq: u64) -> String {
    format!("message:{id}:{seq:020}")
}

fn pending_key(id: &str, seq: u64) -> String {
    format!("message_pending:{id}:{seq:020}")
}

fn history(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    id: &str,
    after: Option<u64>,
    limit: usize,
) -> Result<MessagePage> {
    let last: u64 = optional(table, &format!("message_seq:{id}"))?.unwrap_or(0);
    let start = after.unwrap_or_else(|| last.saturating_sub(limit as u64));
    let mut messages = Vec::new();
    for previous in (start..last).take(limit) {
        messages.push(load::<ConversationMessage>(
            table,
            &message_key(id, previous + 1),
        )?);
    }
    let next_after = messages.last().map_or(start, |m| m.seq);
    Ok(MessagePage {
        messages,
        next_after,
        has_more: next_after < last,
    })
}

fn pending(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    id: &str,
    after: u64,
    limit: usize,
) -> Result<MessagePage> {
    let start = pending_key(id, after);
    let end = pending_key(id, u64::MAX);
    let mut entries = table.range::<&str>((Excluded(start.as_str()), Included(end.as_str())))?;
    let mut messages = Vec::new();
    for entry in entries.by_ref().take(limit) {
        let (_, value) = entry?;
        let seq = serde_json::from_slice(value.value())?;
        messages.push(load::<ConversationMessage>(table, &message_key(id, seq))?);
    }
    let next_after = messages.last().map_or(after, |m| m.seq);
    Ok(MessagePage {
        messages,
        next_after,
        has_more: entries.next().transpose()?.is_some(),
    })
}

impl Store {
    pub fn messages(
        &self,
        id: &str,
        after: Option<u64>,
        limit: usize,
        pending_only: bool,
    ) -> Result<MessagePage> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(RECORDS)?;
        ensure!(
            table.get(format!("org2:{id}").as_str())?.is_some(),
            "not_found: organism"
        );
        let limit = limit.clamp(1, 100);
        if pending_only {
            pending(&table, id, after.unwrap_or(0), limit)
        } else {
            history(&table, id, after, limit)
        }
    }

    /// Both lists reflect the same snapshot even when a reply arrives during a turn.
    pub fn message_context(
        &self,
        id: &str,
    ) -> Result<(Vec<ConversationMessage>, Vec<ConversationMessage>)> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(RECORDS)?;
        Ok((
            pending(&table, id, 0, 1)?.messages,
            history(&table, id, None, 4)?.messages,
        ))
    }

    pub fn send_creator_message(
        &self,
        owner: &str,
        id: &str,
        text: String,
        idempotency_key: &str,
    ) -> Result<ConversationMessage> {
        self.save_message(id, Sender::Creator(owner), text, idempotency_key)
    }

    pub fn reply_to_message(
        &self,
        id: &str,
        reply_to: u64,
        text: String,
        idempotency_key: &str,
    ) -> Result<ConversationMessage> {
        self.save_message(id, Sender::Organism { reply_to }, text, idempotency_key)
    }

    fn save_message(
        &self,
        id: &str,
        sender: Sender<'_>,
        text: String,
        idempotency_key: &str,
    ) -> Result<ConversationMessage> {
        let tx = self.db.begin_write()?;
        let mut table = tx.open_table(RECORDS)?;
        let profile: OrganismProfile = load(&table, &format!("org2:{id}"))?;
        let (role, role_key, reply_to) = match sender {
            Sender::Creator(owner) => {
                ensure!(profile.owner == owner, "not_found: organism");
                (MessageRole::Creator, "creator", None)
            }
            Sender::Organism { reply_to } => (MessageRole::Organism, "organism", Some(reply_to)),
        };
        life_store::check_key(idempotency_key)?;
        let key = format!(
            "message_key:{id}:{role_key}:{}",
            token_hash(idempotency_key)
        );
        if let Some(seq) = optional::<u64>(&table, &key)? {
            let existing: ConversationMessage = load(&table, &message_key(id, seq))?;
            ensure!(
                existing.text == text && existing.reply_to == reply_to,
                "idempotency_conflict"
            );
            return Ok(existing);
        }
        ensure!(
            !text.trim().is_empty() && text.len() <= MESSAGE_TEXT_MAX_BYTES,
            "message text must be nonblank and at most {MESSAGE_TEXT_MAX_BYTES} bytes"
        );
        if let Some(parent) = reply_to {
            let original: ConversationMessage = load(&table, &message_key(id, parent))?;
            ensure!(
                original.role == MessageRole::Creator,
                "reply_to must reference a creator message"
            );
            ensure!(
                table.remove(pending_key(id, parent).as_str())?.is_some(),
                "reply_conflict: creator message already answered"
            );
        }
        let counter = format!("message_seq:{id}");
        let seq = optional::<u64>(&table, &counter)?
            .unwrap_or(0)
            .checked_add(1)
            .context("message sequence overflow")?;
        let message = ConversationMessage {
            seq,
            role,
            text,
            created_at: now_ms(),
            reply_to,
        };
        put(&mut table, &message_key(id, seq), &message)?;
        put(&mut table, &key, &seq)?;
        put(&mut table, &counter, &seq)?;
        if role == MessageRole::Creator {
            put(&mut table, &pending_key(id, seq), &seq)?;
        }
        drop(table);
        tx.commit()?;
        Ok(message)
    }
}

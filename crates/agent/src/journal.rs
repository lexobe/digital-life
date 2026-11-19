use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// Write, sync, rename, then sync the directory before the caller performs a side effect.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .context("file has no name")?
            .to_string_lossy(),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| {
            format!(
                "creating {} (leftover files are retained, not repaired)",
                temp.display()
            )
        })?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Preparing,
    RequestStarted,
    Ready,
    Complete,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionStatus {
    Planned,
    Running,
    Done,
    Uncertain,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Action {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub status: ActionStatus,
    pub result: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Turn {
    pub version: u32,
    pub id: u64,
    pub stage: Stage,
    pub assistant: Option<Value>,
    pub actions: Vec<Action>,
    pub note: Option<String>,
}

pub(crate) struct Journal {
    pub root: PathBuf,
    pub ids: Vec<u64>,
    pub pending: VecDeque<u64>,
    // Held for the lifetime of the agent. Child commands do not inherit this FD.
    _lock: File,
}

impl Journal {
    pub fn open(work: &Path) -> Result<Self> {
        let root = work.join(".agent/history");
        fs::create_dir_all(&root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(work.join(".agent/lock"))?;
        lock.try_lock()
            .context("another agent holds the work directory lock")?;
        let mut ids = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name();
                let name = name.to_str().context("non-UTF8 history directory")?;
                let id: u64 = name
                    .parse()
                    .context("invalid history directory name; refusing to discard history")?;
                ensure!(
                    name == format!("{id:020}"),
                    "noncanonical history directory name"
                );
                ids.push(id);
            }
        }
        ids.sort_unstable();
        let mut journal = Self {
            root,
            ids,
            pending: VecDeque::new(),
            _lock: lock,
        };
        for &id in &journal.ids {
            let path = journal.turn_dir(id).join("state.json");
            if !path.exists() {
                // A crash between mkdir and the first state write cannot have sent a request.
                // Keep the entire orphan directory for inspection; never reuse its ID.
                continue;
            }
            let turn = journal.load(id)?;
            if turn.stage != Stage::Complete {
                journal.pending.push_back(id);
            }
        }
        Ok(journal)
    }

    pub fn turn_dir(&self, id: u64) -> PathBuf {
        self.root.join(format!("{id:020}"))
    }
    pub fn action_dir(&self, id: u64, index: usize) -> PathBuf {
        self.turn_dir(id).join(format!("action-{index:04}"))
    }

    pub fn new_turn(&mut self) -> Result<Turn> {
        let id = self
            .ids
            .last()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .context("turn counter overflow")?;
        fs::create_dir(self.turn_dir(id))?;
        File::open(&self.root)?.sync_all()?;
        let turn = Turn {
            version: 1,
            id,
            stage: Stage::Preparing,
            assistant: None,
            actions: vec![],
            note: None,
        };
        self.save(&turn)?;
        self.ids.push(id);
        self.pending.push_back(id);
        Ok(turn)
    }

    pub fn save(&self, turn: &Turn) -> Result<()> {
        save_json(&self.turn_dir(turn.id).join("state.json"), turn)
    }
    pub fn load(&self, id: u64) -> Result<Turn> {
        let path = self.turn_dir(id).join("state.json");
        let turn: Turn = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("reading {}", path.display()))?;
        ensure!(
            turn.version == 1 && turn.id == id,
            "unsupported or inconsistent journal state: {}",
            path.display()
        );
        Ok(turn)
    }
    pub fn complete(&mut self, turn: &mut Turn) -> Result<()> {
        turn.stage = Stage::Complete;
        self.save(turn)?;
        ensure!(
            self.pending.pop_front() == Some(turn.id),
            "out-of-order journal completion"
        );
        Ok(())
    }
}

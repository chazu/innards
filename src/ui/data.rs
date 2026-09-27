use super::protocol::*;
use anyhow::{Result, bail, ensure};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Complete feeds have O(1) keyed replacements and appends. Ordered deletions
/// are uncommon and may scan the retained keys; paint never walks history.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn generation() -> u64 {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
pub struct Data {
    pub generation: u64,
    pub revision: u64,
    pub total: usize,
    pub retention: usize,
    pub windowed: bool,
    order: VecDeque<String>,
    rows: HashMap<String, Row>,
    positions: HashMap<String, usize>,
    base: usize,
    cache: BTreeMap<usize, Row>,
    pub dropped: usize,
    bytes: usize,
}
fn row_bytes(row: &Row) -> usize {
    row.key.len()
        + row
            .fields
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum::<usize>()
}
fn validate_rows(rows: &[Row]) -> Result<()> {
    let mut keys = HashSet::new();
    ensure!(rows.len() <= MAX_ROWS, "too many rows");
    for row in rows {
        ensure!(
            row_bytes(row) <= 8192 && row.fields.len() <= 64,
            "row payload limit"
        );
        ensure!(
            !row.key.is_empty() && keys.insert(&row.key),
            "empty or duplicate row key"
        );
    }
    Ok(())
}
impl Data {
    pub fn new(c: Collection) -> Result<Self> {
        ensure!(
            !c.id.is_empty() && c.retention > 0 && c.retention <= MAX_ROWS,
            "invalid collection limits"
        );
        ensure!(
            c.start
                .checked_add(c.rows.len())
                .is_some_and(|end| end <= c.total),
            "invalid collection range"
        );
        validate_rows(&c.rows)?;
        if c.windowed {
            ensure!(c.rows.len() <= CACHE_ROWS, "window cache too large");
        } else {
            ensure!(
                c.start == 0 && c.total == c.rows.len() && c.total <= c.retention,
                "incomplete bounded collection"
            );
        }
        let bytes = c.rows.iter().map(row_bytes).sum::<usize>();
        ensure!(bytes <= 8 * 1024 * 1024, "collection byte limit");
        let mut data = Self {
            bytes,
            generation: generation(),
            revision: c.revision,
            total: c.total,
            retention: c.retention,
            windowed: c.windowed,
            order: VecDeque::new(),
            rows: HashMap::new(),
            positions: HashMap::new(),
            base: 0,
            cache: BTreeMap::new(),
            dropped: 0,
        };
        for (i, row) in c.rows.into_iter().enumerate() {
            if data.windowed {
                data.cache.insert(c.start + i, row);
            } else {
                data.positions.insert(row.key.clone(), i);
                data.order.push_back(row.key.clone());
                data.rows.insert(row.key.clone(), row);
            }
        }
        Ok(data)
    }
    pub fn row(&self, index: usize) -> Option<&Row> {
        if self.windowed {
            self.cache.get(&index)
        } else {
            self.order.get(index).and_then(|key| self.rows.get(key))
        }
    }
    pub fn index_of(&self, key: &str) -> Option<usize> {
        if self.windowed {
            self.cache
                .iter()
                .find_map(|(i, r)| (r.key == key).then_some(*i))
        } else {
            self.positions.get(key).map(|i| i - self.base)
        }
    }
    pub fn cached(&self) -> usize {
        if self.windowed {
            self.cache.len()
        } else {
            self.rows.len()
        }
    }
    pub fn window(
        &mut self,
        start: usize,
        total: usize,
        rows: Vec<Row>,
        center: usize,
    ) -> Result<()> {
        ensure!(
            self.windowed && total == self.total && rows.len() <= WINDOW_ROWS,
            "invalid window metadata"
        );
        ensure!(
            start
                .checked_add(rows.len())
                .is_some_and(|end| end <= total),
            "invalid window range"
        );
        validate_rows(&rows)?;
        // Same revision must describe the same stable order, including neighbors.
        for (offset, row) in rows.iter().enumerate() {
            let index = start + offset;
            ensure!(
                !self
                    .cache
                    .iter()
                    .any(|(i, r)| *i != index && r.key == row.key),
                "window reorders a stable revision"
            );
            ensure!(
                self.cache.get(&index).is_none_or(|r| r.key == row.key),
                "window changes a stable key"
            );
        }
        for (offset, row) in rows.into_iter().enumerate() {
            self.cache.insert(start + offset, row);
        }
        while self.cache.len() > CACHE_ROWS {
            let first = *self.cache.first_key_value().unwrap().0;
            let last = *self.cache.last_key_value().unwrap().0;
            self.cache
                .remove(&if first.abs_diff(center) > last.abs_diff(center) {
                    first
                } else {
                    last
                });
        }
        self.generation = generation();
        Ok(())
    }
    pub fn change(&mut self, base: u64, revision: u64, changes: Vec<Change>) -> Result<()> {
        ensure!(
            base == self.revision && revision > base,
            "collection revision mismatch"
        );
        ensure!(
            !self.windowed,
            "windowed changes require a fresh collection descriptor"
        );
        ensure!(changes.len() <= WINDOW_ROWS, "too many operations");
        // Validate the complete transaction against a tiny overlay, not a clone of history.
        let mut overlay = HashMap::<String, bool>::new();
        let mut added = 0;
        let mut sizes = HashMap::<String, usize>::new();
        let mut bytes = self.bytes;
        for change in &changes {
            match change {
                Change::Append { rows } => {
                    validate_rows(rows)?;
                    added += rows.len();
                    ensure!(added <= WINDOW_ROWS, "append batch too large");
                    for row in rows {
                        ensure!(
                            !overlay
                                .get(&row.key)
                                .copied()
                                .unwrap_or_else(|| self.rows.contains_key(&row.key)),
                            "append duplicate key"
                        );
                        bytes += row_bytes(row);
                        sizes.insert(row.key.clone(), row_bytes(row));
                        overlay.insert(row.key.clone(), true);
                    }
                }
                Change::Replace { row } => {
                    validate_rows(std::slice::from_ref(row))?;
                    ensure!(
                        overlay
                            .get(&row.key)
                            .copied()
                            .unwrap_or_else(|| self.rows.contains_key(&row.key)),
                        "replace missing key"
                    );
                    bytes = bytes
                        - sizes
                            .get(&row.key)
                            .copied()
                            .unwrap_or_else(|| self.rows.get(&row.key).map_or(0, row_bytes))
                        + row_bytes(row);
                    sizes.insert(row.key.clone(), row_bytes(row));
                }
                Change::Remove { key } => {
                    if !overlay
                        .get(key)
                        .copied()
                        .unwrap_or_else(|| self.rows.contains_key(key))
                    {
                        bail!("remove missing key");
                    }
                    bytes -= sizes
                        .get(key)
                        .copied()
                        .unwrap_or_else(|| self.rows.get(key).map_or(0, row_bytes));
                    sizes.insert(key.clone(), 0);
                    overlay.insert(key.clone(), false);
                }
            }
        }
        ensure!(
            bytes <= 8 * 1024 * 1024,
            "collection byte limit; producer must trim or resync"
        );
        self.bytes = bytes;
        for change in changes {
            match change {
                Change::Append { rows } => {
                    for row in rows {
                        self.positions
                            .insert(row.key.clone(), self.base + self.order.len());
                        self.order.push_back(row.key.clone());
                        self.rows.insert(row.key.clone(), row);
                    }
                }
                Change::Replace { row } => {
                    self.rows.insert(row.key.clone(), row);
                }
                Change::Remove { key } => {
                    self.rows.remove(&key);
                    self.order.retain(|k| *k != key);
                    self.positions.remove(&key);
                    for (i, key) in self.order.iter().enumerate() {
                        self.positions.insert(key.clone(), self.base + i);
                    }
                }
            }
        }
        while self.order.len() > self.retention {
            let key = self.order.pop_front().unwrap();
            if let Some(row) = self.rows.remove(&key) {
                self.bytes -= row_bytes(&row);
            }
            self.positions.remove(&key);
            self.base += 1;
            self.dropped += 1;
        }
        self.generation = generation();
        self.total = self.order.len();
        self.revision = revision;
        Ok(())
    }
}

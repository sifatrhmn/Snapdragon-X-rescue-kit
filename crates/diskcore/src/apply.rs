use crate::align::align_up;
#[cfg(any(test, feature = "fixtures"))]
use crate::model::FsKind;
use crate::model::{Disk, MIB};
use crate::ops::Op;
use crate::validate::{project, validate, Context, Severity};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    Other(String),
}

pub trait Backend {
    fn snapshot(&mut self) -> Result<Vec<Disk>, BackendError>;
    fn execute(&mut self, op: &Op) -> Result<(), BackendError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    pub completed: Vec<usize>,
    pub failed: Option<(usize, String)>,
    pub not_run: Vec<usize>,
}

fn fail(completed: Vec<usize>, index: usize, message: String, queue_len: usize) -> ApplyReport {
    ApplyReport {
        completed,
        failed: Some((index, message)),
        not_run: (index + 1..queue_len).collect(),
    }
}

/// Compares what the backend produced against the projection. Partitions
/// match by offset, sizes within 1 MiB; the target's filesystem must match
/// for Create and Format.
fn compare(expected: &[Disk], actual: &[Disk], op: &Op) -> Result<(), String> {
    let id = op.disk();
    let exp = expected
        .iter()
        .find(|d| d.id == *id)
        .ok_or_else(|| "expected disk missing from projection".to_string())?;
    let act = actual
        .iter()
        .find(|d| d.id == *id)
        .ok_or_else(|| format!("disk {} is missing from the result", id.0))?;
    if exp.partitions.len() != act.partitions.len() {
        return Err(format!(
            "expected {} partitions, found {}",
            exp.partitions.len(),
            act.partitions.len()
        ));
    }
    let fs_offset = match op {
        Op::Create { offset, .. } => Some(align_up(*offset, MIB)),
        Op::Format { offset, .. } => Some(*offset),
        _ => None,
    };
    for e in &exp.partitions {
        let a = act
            .partitions
            .iter()
            .find(|p| p.offset == e.offset)
            .ok_or_else(|| format!("no partition at offset {}", e.offset))?;
        if a.size.abs_diff(e.size) > MIB {
            return Err(format!(
                "partition at offset {} has size {}, expected {}",
                e.offset, a.size, e.size
            ));
        }
        if fs_offset == Some(e.offset) && a.fs != e.fs {
            return Err(format!(
                "partition at offset {} has filesystem {:?}, expected {:?}",
                e.offset, a.fs, e.fs
            ));
        }
    }
    Ok(())
}

/// Runs the queue against `backend`. The whole queue is validated first;
/// each operation is then executed against a fresh snapshot and its result
/// checked against the projection. Stops at the first error or mismatch.
pub fn apply(backend: &mut dyn Backend, queue: &[Op], ctx: &Context) -> ApplyReport {
    let n = queue.len();
    let initial = match backend.snapshot() {
        Ok(d) => d,
        Err(e) => return fail(Vec::new(), 0, format!("could not read disks: {e}"), n),
    };
    let v = validate(&initial, queue, ctx);
    if let Some(first) = v.issues.iter().find(|i| i.severity == Severity::Error) {
        return fail(
            Vec::new(),
            0,
            format!("operation {}: {}", first.op_index + 1, first.message),
            n,
        );
    }
    let mut completed = Vec::new();
    for (i, op) in queue.iter().enumerate() {
        let before = match backend.snapshot() {
            Ok(d) => d,
            Err(e) => return fail(completed, i, format!("could not read disks: {e}"), n),
        };
        if !before.iter().any(|d| d.id == *op.disk()) {
            let msg = format!("disk {} is no longer present", op.disk().0);
            return fail(completed, i, msg, n);
        }
        let expected = match project(&before, op) {
            Ok(d) => d,
            Err(is) => return fail(completed, i, is.message, n),
        };
        if let Err(e) = backend.execute(op) {
            return fail(completed, i, e.to_string(), n);
        }
        let after = match backend.snapshot() {
            Ok(d) => d,
            Err(e) => return fail(completed, i, format!("could not read disks: {e}"), n),
        };
        if let Err(why) = compare(&expected, &after, op) {
            let msg = format!("result did not match the expected outcome: {why}");
            return fail(completed, i, msg, n);
        }
        completed.push(i);
    }
    ApplyReport {
        completed,
        failed: None,
        not_run: Vec::new(),
    }
}

/// In-memory backend for tests and the simulation mode. Applies operations
/// with `project`, with switches to inject the failures `apply` must survive.
#[cfg(any(test, feature = "fixtures"))]
pub struct FakeBackend {
    pub disks: Vec<Disk>,
    /// Fail the execute call with this zero-based call index.
    pub fail_at: Option<usize>,
    /// After the execute call with this index, the disk identity changes
    /// (as if the device re-enumerated) once `apply` has taken its post-op
    /// snapshot, so the next operation finds its disk missing.
    pub renumber_after: Option<usize>,
    /// Report success without changing anything.
    pub noop: bool,
    /// Execute the operation but leave the target's filesystem as RAW.
    pub wrong_fs: bool,
    /// After executing, add this many bytes to the target's size.
    pub size_skew: i64,
    /// Number of execute calls received so far.
    pub executed: usize,
    renumber_stage: u8,
}

#[cfg(any(test, feature = "fixtures"))]
impl FakeBackend {
    pub fn new(disks: Vec<Disk>) -> Self {
        FakeBackend {
            disks,
            fail_at: None,
            renumber_after: None,
            noop: false,
            wrong_fs: false,
            size_skew: 0,
            executed: 0,
            renumber_stage: 0,
        }
    }
}

#[cfg(any(test, feature = "fixtures"))]
impl Backend for FakeBackend {
    fn snapshot(&mut self) -> Result<Vec<Disk>, BackendError> {
        match self.renumber_stage {
            1 => self.renumber_stage = 2,
            2 => {
                for d in &mut self.disks {
                    d.id.0 = format!("renumbered-{}", d.id.0);
                }
                self.renumber_stage = 0;
            }
            _ => {}
        }
        Ok(self.disks.clone())
    }

    fn execute(&mut self, op: &Op) -> Result<(), BackendError> {
        let idx = self.executed;
        self.executed += 1;
        if self.fail_at == Some(idx) {
            return Err(BackendError::Storage(format!("injected failure at {idx}")));
        }
        if !self.noop {
            self.disks = project(&self.disks, op).map_err(|i| BackendError::Other(i.message))?;
            let target = match op {
                Op::Create { offset, .. } => Some(align_up(*offset, MIB)),
                Op::Format { offset, .. } | Op::Resize { offset, .. } => Some(*offset),
                Op::Delete { .. } => None,
            };
            let part = target.and_then(|t| {
                self.disks
                    .iter_mut()
                    .find(|d| d.id == *op.disk())
                    .and_then(|d| d.partitions.iter_mut().find(|p| p.offset == t))
            });
            if let Some(p) = part {
                if self.wrong_fs {
                    p.fs = FsKind::Raw;
                }
                p.size = p.size.saturating_add_signed(self.size_skew);
            }
        }
        if self.renumber_after == Some(idx) {
            self.renumber_stage = 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{disk_fixture, MIB};

    fn ctx() -> Context {
        Context {
            simulation: true,
            allow_system_partitions: false,
        }
    }

    /// Shrink C: by 10 GiB, create a 10 GiB NTFS partition in the freed
    /// space, then reformat it as exFAT.
    fn queue3() -> (Vec<Disk>, Vec<Op>) {
        let d = disk_fixture();
        let c = &d.partitions[2];
        let new_size = c.size - 10 * 1024 * MIB;
        let freed = c.offset + new_size;
        let q = vec![
            Op::Resize {
                disk: d.id.clone(),
                offset: c.offset,
                new_size,
            },
            Op::Create {
                disk: d.id.clone(),
                offset: freed,
                size: 10 * 1024 * MIB,
                fs: FsKind::Ntfs,
                label: "Data".into(),
            },
            Op::Format {
                disk: d.id.clone(),
                offset: freed,
                fs: FsKind::ExFat,
                label: "X".into(),
            },
        ];
        (vec![d], q)
    }

    #[test]
    fn apply_runs_whole_queue() {
        let (disks, q) = queue3();
        let mut fake = FakeBackend::new(disks);
        let r = apply(&mut fake, &q, &ctx());
        assert_eq!(r.completed, vec![0, 1, 2]);
        assert_eq!(r.failed, None);
        assert!(r.not_run.is_empty());
        assert_eq!(fake.executed, 3);
        assert_eq!(fake.disks[0].partitions.len(), 5);
    }

    #[test]
    fn apply_stops_at_failure() {
        let (disks, q) = queue3();
        let mut fake = FakeBackend::new(disks);
        fake.fail_at = Some(1);
        let r = apply(&mut fake, &q, &ctx());
        assert_eq!(r.completed, vec![0]);
        assert!(matches!(r.failed, Some((1, _))));
        assert_eq!(r.not_run, vec![2]);
        assert_eq!(fake.executed, 2);
    }

    #[test]
    fn apply_detects_renumbered_disk() {
        let (disks, q) = queue3();
        let mut fake = FakeBackend::new(disks);
        fake.renumber_after = Some(0);
        let r = apply(&mut fake, &q, &ctx());
        assert_eq!(r.completed, vec![0]);
        let (idx, msg) = r.failed.expect("must fail");
        assert_eq!(idx, 1);
        assert!(msg.contains("no longer present"), "{msg}");
        assert_eq!(r.not_run, vec![2]);
        assert_eq!(fake.executed, 1);
    }

    #[test]
    fn apply_detects_mismatch() {
        let (disks, q) = queue3();
        let mut fake = FakeBackend::new(disks);
        fake.noop = true;
        let r = apply(&mut fake, &q, &ctx());
        let (idx, msg) = r.failed.expect("must fail");
        assert_eq!(idx, 0);
        assert!(msg.contains("did not match"), "{msg}");
        assert!(r.completed.is_empty());
        assert_eq!(r.not_run, vec![1, 2]);
    }

    #[test]
    fn apply_detects_wrong_fs_on_format() {
        let (disks, q) = queue3();
        let mut fake = FakeBackend::new(disks);
        // Run the first two ops cleanly, then break the Format.
        let r = apply(&mut fake, &q[..2], &ctx());
        assert_eq!(r.failed, None);
        fake.wrong_fs = true;
        let r = apply(&mut fake, &q[2..], &ctx());
        let (idx, msg) = r.failed.expect("must fail");
        assert_eq!(idx, 0);
        assert!(msg.contains("did not match"), "{msg}");
        assert!(msg.contains("filesystem"), "{msg}");
    }

    #[test]
    fn apply_detects_wrong_fs_on_create() {
        let (disks, q) = queue3();
        let mut fake = FakeBackend::new(disks);
        let r = apply(&mut fake, &q[..1], &ctx());
        assert_eq!(r.failed, None);
        fake.wrong_fs = true;
        let r = apply(&mut fake, &q[1..2], &ctx());
        let (_, msg) = r.failed.expect("must fail");
        assert!(msg.contains("did not match"), "{msg}");
        assert!(msg.contains("filesystem"), "{msg}");
    }

    #[test]
    fn apply_size_tolerance_boundary() {
        let (disks, q) = queue3();
        let mib = MIB as i64;
        for skew in [mib, -mib] {
            let mut ok = FakeBackend::new(disks.clone());
            ok.size_skew = skew;
            assert_eq!(apply(&mut ok, &q[..1], &ctx()).failed, None, "skew {skew}");
        }
        for skew in [mib + 1, -mib - 1] {
            let mut bad = FakeBackend::new(disks.clone());
            bad.size_skew = skew;
            let (_, msg) = apply(&mut bad, &q[..1], &ctx()).failed.expect("must fail");
            assert!(msg.contains("did not match"), "{msg}");
        }
    }

    #[test]
    fn apply_refuses_invalid_queue_without_executing() {
        let (mut disks, q) = queue3();
        disks[0].partitions[2].protection = crate::model::Protection::Locked;
        let mut fake = FakeBackend::new(disks);
        let r = apply(&mut fake, &q, &ctx());
        assert!(matches!(r.failed, Some((0, _))));
        assert!(r.completed.is_empty());
        assert_eq!(fake.executed, 0);
    }
}

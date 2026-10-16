use std::path::{Path, PathBuf};

use notify::event::{EventKind, ModifyKind};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};

use crate::backend::{EventSink, Incoming, RawEvent, RawKind, SourceFactory, WatchSource};
use crate::error::FileObserverError;

/// `notify`-backed watch source. FSEvents and inotify report resolved paths,
/// so events are mapped back under the root the caller subscribed with.
pub struct NotifySource {
    watcher: Option<RecommendedWatcher>,
}

impl NotifySource {
    pub fn new(root: &Path, sink: EventSink) -> Result<Self, FileObserverError> {
        let given = root.to_path_buf();
        let real = std::fs::canonicalize(root).unwrap_or_else(|_| given.clone());
        let watcher = RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| match result {
                Ok(event) => {
                    for raw in map_event(event, &real, &given) {
                        sink(Incoming::Event(raw));
                    }
                }
                Err(error) => sink(Incoming::Error(error.into())),
            },
            Config::default(),
        )?;
        Ok(Self {
            watcher: Some(watcher),
        })
    }

    pub fn factory(root: PathBuf) -> SourceFactory {
        Box::new(move |sink| Ok(Box::new(NotifySource::new(&root, sink)?) as Box<dyn WatchSource>))
    }
}

impl WatchSource for NotifySource {
    fn watch(&mut self, dir: &Path, recursive: bool) -> Result<(), FileObserverError> {
        let Some(watcher) = self.watcher.as_mut() else {
            return Err(FileObserverError::Closed);
        };
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(dir, mode).map_err(Into::into)
    }

    fn unwatch(&mut self, dir: &Path) {
        if let Some(watcher) = self.watcher.as_mut() {
            let _ = watcher.unwatch(dir);
        }
    }

    fn close(&mut self) {
        self.watcher = None;
    }
}

pub(crate) fn map_event(event: notify::Event, real: &Path, given: &Path) -> Vec<RawEvent> {
    let kind = match event.kind {
        EventKind::Access(_) => return Vec::new(),
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
            RawKind::Rename
        }
        EventKind::Modify(_) | EventKind::Any | EventKind::Other => RawKind::Change,
    };
    if event.need_rescan() || event.paths.is_empty() {
        return vec![RawEvent { kind, path: None }];
    }
    event
        .paths
        .into_iter()
        .map(|path| RawEvent {
            kind,
            path: Some(rebase(&path, real, given)),
        })
        .collect()
}

fn rebase(path: &Path, real: &Path, given: &Path) -> PathBuf {
    match path.strip_prefix(real) {
        Ok(rest) if rest.as_os_str().is_empty() => given.to_path_buf(),
        Ok(rest) => given.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, Flag, RemoveKind, RenameMode};

    #[test]
    fn maps_kinds_and_rebases_resolved_paths() {
        let real = Path::new("/private/tmp/r");
        let given = Path::new("/tmp/r");
        let cases = [
            (EventKind::Create(CreateKind::File), RawKind::Rename),
            (EventKind::Remove(RemoveKind::File), RawKind::Rename),
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
                RawKind::Rename,
            ),
            (
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                RawKind::Change,
            ),
        ];
        for (kind, expected) in cases {
            let event = notify::Event::new(kind).add_path(PathBuf::from("/private/tmp/r/a/b"));
            assert_eq!(
                map_event(event, real, given),
                vec![RawEvent {
                    kind: expected,
                    path: Some(PathBuf::from("/tmp/r/a/b"))
                }]
            );
        }
    }

    #[test]
    fn drops_access_and_maps_rescan_to_pathless() {
        let real = Path::new("/r");
        let access = notify::Event::new(EventKind::Access(notify::event::AccessKind::Any))
            .add_path(PathBuf::from("/r/x"));
        assert!(map_event(access, real, real).is_empty());
        let rescan = notify::Event::new(EventKind::Other)
            .add_path(PathBuf::from("/r"))
            .set_flag(Flag::Rescan);
        assert_eq!(
            map_event(rescan, real, real),
            vec![RawEvent {
                kind: RawKind::Change,
                path: None
            }]
        );
    }
}

//! Undo/redo by snapshots of the whole show. An edit is recorded once it
//! settles (mouse released and no change for a moment), so one drag or one
//! burst of typing is a single undo step.

use mapforge_core::ShowProject;
use std::time::{Duration, Instant};

const MAX_STEPS: usize = 200;
const SETTLE: Duration = Duration::from_millis(400);

pub struct History {
    undo: Vec<ShowProject>,
    redo: Vec<ShowProject>,
    /// The show as of the last recorded step.
    base: ShowProject,
    /// The show as seen last frame, to notice when editing pauses.
    seen: ShowProject,
    last_change: Instant,
}

impl History {
    pub fn new(project: &ShowProject) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            base: project.clone(),
            seen: project.clone(),
            last_change: Instant::now(),
        }
    }

    /// Forgets all steps, e.g. after opening another show.
    pub fn reset(&mut self, project: &ShowProject) {
        *self = Self::new(project);
    }

    /// Call once per frame after the UI has run.
    pub fn track(&mut self, project: &ShowProject, pointer_down: bool) {
        if *project != self.seen {
            self.seen = project.clone();
            self.last_change = Instant::now();
        }
        if !pointer_down && self.last_change.elapsed() >= SETTLE {
            self.commit(project);
        }
    }

    fn commit(&mut self, project: &ShowProject) {
        if *project == self.base {
            return;
        }
        self.undo
            .push(std::mem::replace(&mut self.base, project.clone()));
        if self.undo.len() > MAX_STEPS {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn can_undo(&self, project: &ShowProject) -> bool {
        !self.undo.is_empty() || *project != self.base
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self, project: &mut ShowProject) -> bool {
        self.commit(project);
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(project, previous));
        self.settle_on(project);
        true
    }

    pub fn redo(&mut self, project: &mut ShowProject) -> bool {
        self.commit(project);
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(project, next));
        self.settle_on(project);
        true
    }

    fn settle_on(&mut self, project: &ShowProject) {
        self.base = project.clone();
        self.seen = project.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renamed(name: &str) -> ShowProject {
        ShowProject {
            name: name.into(),
            ..ShowProject::default()
        }
    }

    #[test]
    fn undo_and_redo_walk_through_steps() {
        let mut project = renamed("A");
        let mut history = History::new(&project);
        project.name = "B".into();
        assert!(history.undo(&mut project));
        assert_eq!(project.name, "A");
        assert!(history.redo(&mut project));
        assert_eq!(project.name, "B");
        assert!(!history.redo(&mut project));
    }

    #[test]
    fn a_new_edit_clears_redo() {
        let mut project = renamed("A");
        let mut history = History::new(&project);
        project.name = "B".into();
        history.undo(&mut project);
        project.name = "C".into();
        history.undo(&mut project);
        assert_eq!(project.name, "A");
        assert!(history.redo(&mut project));
        assert_eq!(project.name, "C");
        assert!(!history.can_redo());
    }
}

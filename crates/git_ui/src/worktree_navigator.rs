use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use collections::{BTreeSet, HashMap, HashSet};
use git::repository::{Worktree as GitWorktree, original_repo_path_from_common_dir};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel, Render, ScrollHandle,
    Subscription, Task, WeakEntity, Window,
};
use project::git_store::{GitStoreEvent, RepositoryEvent};
use serde::{Deserialize, Serialize};
use settings::{GitDiffBaseSetting, SidebarSide};
use ui::{CommonAnimationExt, ContextMenu, ListItem, Tooltip, prelude::*, right_click_menu};
use util::{ResultExt, path_list::PathList};
use workspace::{
    MultiWorkspace, MultiWorkspaceEvent, OpenMode, ProjectGroupKey, RemovalIntent, Sidebar,
    SidebarEvent, Workspace,
};

use crate::{branch_diff::BranchDiff, project_diff::ProjectDiff};

#[derive(PartialEq, Eq)]
struct ProjectEntry {
    key: ProjectGroupKey,
    repository_dir: Option<PathBuf>,
    worktrees: Vec<GitWorktree>,
    error: Option<String>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum NavigatorProjectId {
    Repository(PathBuf),
    Folders(Vec<PathBuf>),
}

impl NavigatorProjectId {
    fn root(&self) -> Option<&Path> {
        match self {
            Self::Repository(directory)
                if directory.file_name().is_some_and(|name| name == ".git") =>
            {
                directory.parent()
            }
            Self::Repository(directory) => Some(directory.as_path()),
            Self::Folders(paths) => paths.first().map(PathBuf::as_path),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self == other
            || self
                .root()
                .zip(other.root())
                .is_some_and(|(left, right)| left == right)
    }
}

impl ProjectEntry {
    fn id(&self) -> NavigatorProjectId {
        match &self.repository_dir {
            Some(directory) => NavigatorProjectId::Repository(directory.clone()),
            None => NavigatorProjectId::Folders(self.key.path_list().paths().to_vec()),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SerializedNavigator {
    width: Option<f32>,
    pinned_worktrees: Vec<PathBuf>,
    hidden_worktrees: BTreeSet<PathBuf>,
    hidden_projects: BTreeSet<NavigatorProjectId>,
    collapsed_projects: BTreeSet<NavigatorProjectId>,
}

struct WorktreeRow {
    path: PathBuf,
    label: SharedString,
    can_delete: bool,
}

pub struct WorktreeNavigator {
    multi_workspace: WeakEntity<MultiWorkspace>,
    focus_handle: FocusHandle,
    width: Pixels,
    scroll: ScrollHandle,
    projects: Vec<ProjectEntry>,
    requested_path: Option<PathBuf>,
    loading_paths: HashSet<PathBuf>,
    deleting_paths: HashSet<PathBuf>,
    pinned_worktrees: Vec<PathBuf>,
    hidden_worktrees: BTreeSet<PathBuf>,
    hidden_projects: BTreeSet<NavigatorProjectId>,
    collapsed_projects: BTreeSet<NavigatorProjectId>,
    error: Option<String>,
    refresh_task: Option<Task<()>>,
    open_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
    repository_subscriptions: HashMap<gpui::EntityId, Subscription>,
}

impl WorktreeNavigator {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.on_release(|this, _| {
                for task in std::mem::take(&mut this.open_tasks) {
                    task.detach();
                }
            }),
            cx.subscribe_in(
                &multi_workspace,
                window,
                |this, _, event, window, cx| match event {
                    MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } => cx.notify(),
                    MultiWorkspaceEvent::WorkspaceAdded(_)
                    | MultiWorkspaceEvent::WorkspaceRemoved(_)
                    | MultiWorkspaceEvent::ProjectGroupsChanged => this.refresh(window, cx),
                },
            ),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.refresh(window, cx);
                }
            }),
        ];
        cx.defer_in(window, |this, window, cx| this.refresh(window, cx));
        Self {
            multi_workspace: multi_workspace.downgrade(),
            focus_handle: cx.focus_handle(),
            width: px(240.),
            scroll: ScrollHandle::new(),
            projects: Vec::new(),
            requested_path: None,
            loading_paths: HashSet::default(),
            deleting_paths: HashSet::default(),
            pinned_worktrees: Vec::new(),
            hidden_worktrees: BTreeSet::default(),
            hidden_projects: BTreeSet::default(),
            collapsed_projects: BTreeSet::default(),
            error: None,
            refresh_task: None,
            open_tasks: Vec::new(),
            _subscriptions: subscriptions,
            repository_subscriptions: HashMap::default(),
        }
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let groups = multi_workspace.read(cx).project_groups(cx);
        let mut projects: Vec<ProjectEntry> = Vec::new();
        let mut observed_stores = HashSet::default();
        for group in groups
            .into_iter()
            .filter(|group| group.key.host().is_none())
        {
            let representative = group
                .workspaces
                .iter()
                .find(|workspace| workspace.read(cx).project().read(cx).is_local());
            let repository = representative
                .and_then(|workspace| workspace.read(cx).project().read(cx).active_repository(cx));
            if let Some(workspace) = representative {
                let git_store = workspace.read(cx).project().read(cx).git_store().clone();
                let store_id = git_store.entity_id();
                observed_stores.insert(store_id);
                self.repository_subscriptions
                    .entry(store_id)
                    .or_insert_with(|| {
                        cx.subscribe_in(&git_store, window, |this, _, event, window, cx| {
                            if matches!(
                                event,
                                GitStoreEvent::RepositoryAdded
                                    | GitStoreEvent::RepositoryRemoved(_)
                                    | GitStoreEvent::RepositoryUpdated(
                                        _,
                                        RepositoryEvent::GitWorktreeListChanged
                                            | RepositoryEvent::HeadChanged,
                                        _
                                    )
                            ) {
                                this.refresh(window, cx);
                            }
                        })
                    });
            }
            let mut worktrees = Vec::new();
            let mut repository_dir = None;
            if let Some(repository) = repository {
                // The Git store already discovers linked worktrees during its scans.
                // Reading that snapshot avoids spawning another Git process per project.
                let snapshot = repository.read(cx).snapshot();
                repository_dir = Some(snapshot.common_dir_abs_path.to_path_buf());
                worktrees.extend(snapshot.linked_worktrees().iter().cloned());
                worktrees.push(GitWorktree {
                    path: snapshot.work_directory_abs_path.to_path_buf(),
                    ref_name: snapshot
                        .branch
                        .as_ref()
                        .map(|branch| branch.name().to_string().into()),
                    sha: snapshot
                        .head_commit
                        .as_ref()
                        .map(|commit| commit.sha.clone())
                        .unwrap_or_default(),
                    is_main: snapshot.is_main_worktree(),
                    is_bare: false,
                });
            }
            // Workspace layouts may include extra roots, but they still share
            // one Git repository and must not repeat its worktree list.
            if repository_dir.is_some()
                && let Some(project) = projects
                    .iter_mut()
                    .find(|project| project.repository_dir == repository_dir)
            {
                project.worktrees.extend(worktrees);
                sort_worktrees(&mut project.worktrees);
            } else {
                sort_worktrees(&mut worktrees);
                projects.push(ProjectEntry {
                    key: group.key,
                    repository_dir,
                    worktrees,
                    error: None,
                });
            }
        }
        self.repository_subscriptions
            .retain(|id, _| observed_stores.contains(id));
        let projects = normalize_projects(projects);
        if self.projects != projects {
            self.projects = projects;
            cx.notify();
        }
    }

    fn refresh_from_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A burst of refresh clicks should not enqueue duplicate Git jobs. Dropping
        // the receiver does not cancel a job already in the repository's queue.
        if self
            .refresh_task
            .as_ref()
            .is_some_and(|task| !task.is_ready())
        {
            return;
        }
        self.refresh(window, cx);
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let groups = multi_workspace.read(cx).project_groups(cx);
        let mut requested_repositories = HashSet::default();
        let requests: Vec<_> = groups
            .into_iter()
            .filter(|group| group.key.host().is_none())
            .filter_map(|group| {
                let repository = group.workspaces.iter().find_map(|workspace| {
                    let project = workspace.read(cx).project().read(cx);
                    project
                        .is_local()
                        .then(|| project.active_repository(cx))
                        .flatten()
                })?;
                let repository_dir = repository.read(cx).common_dir_abs_path.to_path_buf();
                if !requested_repositories.insert(repository_dir.clone()) {
                    return None;
                }
                Some((
                    repository_dir,
                    repository.update(cx, |repo, _| repo.worktrees()),
                ))
            })
            .collect();
        self.refresh_task =
            Some(cx.spawn(async move |this, cx| {
                let updates = futures::future::join_all(requests.into_iter().map(
                    |(repository_dir, request)| async move {
                        let result = request
                            .await
                            .context("Git worktree request cancelled")
                            .and_then(|result| result);
                        let (worktrees, error) = match result {
                            Ok(mut worktrees) => {
                                sort_worktrees(&mut worktrees);
                                (worktrees, None)
                            }
                            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                        };
                        (repository_dir, worktrees, error)
                    },
                ))
                .await;
                this.update(cx, |this, cx| {
                    let mut changed = false;
                    for (repository_dir, worktrees, error) in updates {
                        if let Some(project) = this.projects.iter_mut().find(|project| {
                            project.repository_dir.as_ref() == Some(&repository_dir)
                        }) && (project.worktrees != worktrees || project.error != error)
                        {
                            project.worktrees = worktrees;
                            project.error = error;
                            changed = true;
                        }
                    }
                    if changed {
                        cx.notify();
                    }
                })
                .log_err();
            }));
    }

    fn state_changed(&self, cx: &mut Context<Self>) {
        cx.emit(SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn toggle_pin(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.pinned_worktrees.contains(&path) {
            self.pinned_worktrees.retain(|pinned| pinned != &path);
        } else {
            self.hidden_worktrees.remove(&path);
            self.pinned_worktrees.push(path);
        }
        self.state_changed(cx);
    }

    fn remove_worktree_entry(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.pinned_worktrees.retain(|pinned| pinned != &path);
        self.hidden_worktrees.insert(path);
        self.state_changed(cx);
    }

    fn remove_project_entry(&mut self, project: NavigatorProjectId, cx: &mut Context<Self>) {
        self.hidden_projects.insert(project);
        self.state_changed(cx);
    }

    fn is_project_hidden(&self, project: &ProjectEntry) -> bool {
        let id = project.id();
        self.hidden_projects
            .iter()
            .any(|hidden| hidden.matches(&id))
    }

    fn restore_removed_entries(&mut self, cx: &mut Context<Self>) {
        self.hidden_worktrees.clear();
        self.hidden_projects.clear();
        self.state_changed(cx);
    }

    fn worktree_rows(&self, project: &ProjectEntry) -> Vec<WorktreeRow> {
        let mut rows: Vec<_> = if project.repository_dir.is_none() {
            project
                .key
                .path_list()
                .paths()
                .iter()
                .map(|path| WorktreeRow {
                    path: path.clone(),
                    label: path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                    can_delete: false,
                })
                .collect()
        } else {
            project
                .worktrees
                .iter()
                .map(|worktree| WorktreeRow {
                    path: worktree.path.clone(),
                    label: worktree_label(worktree),
                    can_delete: !worktree.is_main && !worktree.is_bare,
                })
                .collect()
        };
        rows.retain(|row| !self.hidden_worktrees.contains(&row.path));
        rows.sort_by_key(|row| {
            self.pinned_worktrees
                .iter()
                .position(|path| path == &row.path)
                .unwrap_or(usize::MAX)
        });
        rows
    }

    fn delete_worktree(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading_paths.contains(&path) || self.deleting_paths.contains(&path) {
            return;
        }
        let candidate = self.projects.iter().find_map(|project| {
            let worktree = project
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)?;
            if worktree.is_main || worktree.is_bare {
                return None;
            }
            Some((project.repository_dir.as_ref()?, worktree_label(worktree)))
        });
        let Some((repository_dir, label)) = candidate else {
            self.error = Some(
                "Only linked Git worktrees can be deleted. The main checkout is protected.".into(),
            );
            cx.notify();
            return;
        };
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let repository = multi_workspace.read(cx).workspaces().find_map(|workspace| {
            let repository = workspace
                .read(cx)
                .project()
                .read(cx)
                .active_repository(cx)?;
            (repository.read(cx).common_dir_abs_path.as_ref() == repository_dir.as_path())
                .then_some(repository)
        });
        let Some(repository) = repository else {
            self.error = Some("Open this project before deleting a worktree.".into());
            cx.notify();
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete worktree \"{label}\"?"),
            Some(&format!("Permanently delete {}. Its Git branch will be kept. Workspaces containing this folder, including their terminals and agents, will close. Git will refuse deletion if the worktree has uncommitted changes.", path.display())),
            &["Cancel", "Delete worktree"],
            cx,
        );
        self.error = None;
        self.deleting_paths.insert(path.clone());
        cx.notify();
        self.open_tasks.retain(|task| !task.is_ready());
        self.open_tasks
            .push(cx.spawn_in(window, async move |this, cx| {
                let result: Result<bool> = async {
                    if answer.await? != 1 {
                        return Ok(false);
                    }
                    let removal =
                        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                            let workspaces: Vec<_> = multi_workspace
                                .workspaces()
                                .filter(|workspace| {
                                    workspace
                                        .read(cx)
                                        .root_paths(cx)
                                        .iter()
                                        .any(|root| root.as_ref() == path.as_path())
                                })
                                .cloned()
                                .collect();
                            if workspaces.is_empty() {
                                None
                            } else {
                                Some(multi_workspace.remove(
                                    workspaces,
                                    RemovalIntent::KeepProject,
                                    window,
                                    cx,
                                ))
                            }
                        })?;
                    if let Some(removal) = removal
                        && !removal.await?
                    {
                        return Ok(false);
                    }
                    repository
                        .update(cx, |repository, _| {
                            repository.remove_worktree(path.clone(), false)
                        })
                        .await
                        .context("Git worktree deletion was cancelled")??;
                    Ok(true)
                }
                .await;
                this.update_in(cx, |this, window, cx| {
                    this.deleting_paths.remove(&path);
                    match result {
                        Ok(true) => {
                            this.pinned_worktrees.retain(|pinned| pinned != &path);
                            this.hidden_worktrees.remove(&path);
                            for project in &mut this.projects {
                                project.worktrees.retain(|worktree| worktree.path != path);
                            }
                            this.state_changed(cx);
                            this.refresh_from_git(window, cx);
                        }
                        Ok(false) => {}
                        Err(error) => {
                            this.error = Some(format!("Could not delete worktree: {error:#}"))
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }));
    }

    fn select_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.deleting_paths.contains(&path) {
            return;
        }
        let mut restored = self.hidden_worktrees.remove(&path);
        for project in &self.projects {
            if project
                .worktrees
                .iter()
                .any(|worktree| worktree.path == path)
                || project.key.path_list().paths().contains(&path)
            {
                let id = project.id();
                let before = self.hidden_projects.len();
                self.hidden_projects.retain(|hidden| !hidden.matches(&id));
                restored |= before != self.hidden_projects.len();
            }
        }
        if restored {
            cx.emit(SidebarEvent::SerializeNeeded);
        }
        self.requested_path = Some(path.clone());
        self.error = None;
        if self
            .multi_workspace
            .upgrade()
            .is_some_and(|multi_workspace| {
                let workspace = multi_workspace.read(cx).workspace().read(cx);
                workspace.active_item(cx).is_some()
                    && workspace
                        .root_paths(cx)
                        .iter()
                        .any(|root| root.as_ref() == path.as_path())
            })
        {
            cx.notify();
            return;
        }
        cx.notify();
        // Keep in-flight opens alive. Cancelling a workspace load halfway through
        // can leave it registered without completing its panel restoration.
        if !self.loading_paths.insert(path.clone()) {
            return;
        }
        self.open_tasks.retain(|task| !task.is_ready());
        let multi_workspace = self.multi_workspace.clone();
        let source = match multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.retain_active_workspace(cx);
            multi_workspace.workspace().clone()
        }) {
            Ok(source) => source,
            Err(error) => {
                self.loading_paths.remove(&path);
                self.error = Some(format!("{error:#}"));
                return;
            }
        };
        let app_state = source.read(cx).app_state().clone();
        let dock_state = source.read(cx).capture_dock_state(window, cx);
        let window_handle = window.window_handle().downcast::<MultiWorkspace>();
        self.open_tasks
            .push(cx.spawn_in(window, async move |this, cx| {
                let result: Result<Entity<Workspace>> = async {
                    let metadata = app_state.fs.metadata(&path).await?.with_context(|| {
                        format!("Worktree no longer exists: {}", path.display())
                    })?;
                    ensure!(
                        metadata.is_dir,
                        "Not a project directory: {}",
                        path.display()
                    );
                    let canonical = app_state.fs.canonicalize(&path).await?;
                    let existing = multi_workspace.update(cx, |multi_workspace, cx| {
                        multi_workspace.workspace_for_paths(
                            &PathList::new(std::slice::from_ref(&canonical)),
                            None,
                            cx,
                        )
                    })?;
                    let workspace = if let Some(existing) = existing {
                        existing
                    } else {
                        cx.update(|_, cx| {
                            Workspace::new_local(
                                vec![canonical],
                                app_state,
                                window_handle,
                                None,
                                Some(Box::new(move |workspace, window, cx| {
                                    workspace.set_dock_structure(dock_state, window, cx);
                                })),
                                OpenMode::Add,
                                cx,
                            )
                        })?
                        .await?
                        .workspace
                    };
                    if let Some(panels) =
                        workspace.update(cx, |workspace, _| workspace.take_panels_task())
                    {
                        panels.await?;
                    }
                    Ok(workspace)
                }
                .await;
                this.update_in(cx, |this, window, cx| {
                    this.loading_paths.remove(&path);
                    if this.requested_path.as_ref() != Some(&path) {
                        if let Err(error) = result {
                            log::error!("Background worktree open failed: {error:#}");
                        }
                        cx.notify();
                        return;
                    }
                    match result {
                        Ok(workspace) => {
                            multi_workspace
                                .update(cx, |multi_workspace, cx| {
                                    multi_workspace.activate(workspace.clone(), None, window, cx);
                                })
                                .log_err();
                            workspace.update(cx, |workspace, cx| {
                                if workspace.active_item(cx).is_none() {
                                    if workspace
                                        .project()
                                        .read(cx)
                                        .git_store()
                                        .read(cx)
                                        .diff_base()
                                        == GitDiffBaseSetting::DefaultBranch
                                    {
                                        BranchDiff::deploy_branch_diff(workspace, window, cx);
                                    } else {
                                        ProjectDiff::deploy_at(workspace, None, window, cx);
                                    }
                                }
                            });
                        }
                        Err(error) => this.error = Some(format!("{error:#}")),
                    }
                    this.refresh(window, cx);
                    cx.notify();
                })
                .log_err();
            }));
    }

    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            match paths
                .await
                .context("Project picker was cancelled")
                .and_then(|result| result)
            {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.into_iter().next() {
                        this.update_in(cx, |this, window, cx| this.select_path(path, window, cx))
                            .log_err();
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("{error:#}"));
                        cx.notify();
                    })
                    .log_err();
                }
            }
        })
        .detach();
    }
}

fn normalize_projects(projects: Vec<ProjectEntry>) -> Vec<ProjectEntry> {
    let mut known_paths: HashSet<_> = projects
        .iter()
        .filter(|project| project.repository_dir.is_some())
        .flat_map(|project| {
            project
                .worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
        })
        .collect();
    known_paths.extend(projects.iter().filter_map(|project| {
        original_repo_path_from_common_dir(project.repository_dir.as_ref()?)
    }));
    let mut normalized = Vec::new();
    for project in projects {
        if project.repository_dir.is_some() {
            normalized.push(project);
        } else {
            // Restored groups can have no live workspace yet. Split their roots
            // so older multi-folder layouts do not repeat a known repository.
            for path in project.key.path_list().paths() {
                if known_paths.insert(path.clone()) {
                    normalized.push(ProjectEntry {
                        key: ProjectGroupKey::new(None, PathList::new(std::slice::from_ref(path))),
                        repository_dir: None,
                        worktrees: Vec::new(),
                        error: None,
                    });
                }
            }
        }
    }
    normalized
}

fn sort_worktrees(worktrees: &mut Vec<GitWorktree>) {
    worktrees.retain(|worktree| !worktree.is_bare);
    worktrees.sort_by(|a, b| b.is_main.cmp(&a.is_main).then_with(|| a.path.cmp(&b.path)));
    worktrees.dedup_by(|a, b| a.path == b.path);
}

fn worktree_label(worktree: &GitWorktree) -> SharedString {
    worktree
        .ref_name
        .as_ref()
        .map(|name| {
            SharedString::from(name.strip_prefix("refs/heads/").unwrap_or(name).to_string())
        })
        .unwrap_or_else(|| {
            let name = worktree
                .path
                .file_name()
                .unwrap_or(worktree.path.as_os_str())
                .to_string_lossy();
            format!("{name} (detached)").into()
        })
}

fn project_label(project: &ProjectEntry) -> SharedString {
    if let Some(directory) = &project.repository_dir {
        let root =
            original_repo_path_from_common_dir(directory).unwrap_or_else(|| directory.clone());
        return root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
            .into_owned()
            .into();
    }
    project
        .key
        .path_list()
        .paths()
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
        })
        .collect::<Vec<_>>()
        .join(", ")
        .into()
}

impl Focusable for WorktreeNavigator {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<SidebarEvent> for WorktreeNavigator {}

impl Sidebar for WorktreeNavigator {
    fn width(&self, _: &App) -> Pixels {
        self.width
    }
    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(px(240.)).clamp(px(180.), px(500.));
        cx.notify();
    }
    fn has_notifications(&self, _: &App) -> bool {
        false
    }
    fn side(&self, _: &App) -> SidebarSide {
        SidebarSide::Right
    }
    fn is_threads_list_view_active(&self) -> bool {
        false
    }
    fn serialized_state(&self, _: &App) -> Option<String> {
        serde_json::to_string(&SerializedNavigator {
            width: Some(self.width.into()),
            pinned_worktrees: self.pinned_worktrees.clone(),
            hidden_worktrees: self.hidden_worktrees.clone(),
            hidden_projects: self.hidden_projects.clone(),
            collapsed_projects: self.collapsed_projects.clone(),
        })
        .log_err()
    }
    fn restore_serialized_state(&mut self, state: &str, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = serde_json::from_str::<SerializedNavigator>(state).log_err() {
            self.pinned_worktrees = state.pinned_worktrees;
            self.hidden_worktrees = state.hidden_worktrees;
            self.hidden_projects = state.hidden_projects;
            self.collapsed_projects = state.collapsed_projects;
            self.set_width(state.width.map(px), cx);
        }
    }
}

impl Render for WorktreeNavigator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let multi_workspace = self.multi_workspace.upgrade();
        let active_path = multi_workspace.as_ref().and_then(|multi_workspace| {
            multi_workspace
                .read(cx)
                .workspace()
                .read(cx)
                .root_paths(cx)
                .into_iter()
                .next()
        });
        let mut rows = Vec::new();
        for (index, project) in self.projects.iter().enumerate() {
            let project_id = project.id();
            if self.is_project_hidden(project) {
                continue;
            }
            let expanded = !self
                .collapsed_projects
                .iter()
                .any(|collapsed| collapsed.matches(&project_id));
            let remove_id = project_id.clone();
            rows.push(
                ListItem::new(("navigator-project", index))
                    .start_slot(Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    }))
                    .child(Label::new(project_label(project)).truncate())
                    .end_slot(
                        IconButton::new(("remove-project", index), IconName::Close)
                            .icon_size(IconSize::Small)
                            .aria_label("Remove project from sidebar")
                            .tooltip(Tooltip::text(
                                "Remove project from sidebar (keep files and sessions)",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_project_entry(remove_id.clone(), cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this
                            .collapsed_projects
                            .iter()
                            .any(|collapsed| collapsed.matches(&project_id))
                        {
                            this.collapsed_projects
                                .retain(|collapsed| !collapsed.matches(&project_id));
                        } else {
                            this.collapsed_projects.insert(project_id.clone());
                        }
                        this.state_changed(cx);
                    }))
                    .into_any_element(),
            );
            if !expanded {
                continue;
            }
            for (row, worktree) in self.worktree_rows(project).into_iter().enumerate() {
                let WorktreeRow {
                    path,
                    label,
                    can_delete,
                } = worktree;
                let selected = active_path.as_deref() == Some(path.as_path());
                let loading =
                    self.loading_paths.contains(&path) || self.deleting_paths.contains(&path);
                let pinned = self.pinned_worktrees.contains(&path);
                let pin_path = path.clone();
                let delete_path = path.clone();
                let menu_path = path.clone();
                let tooltip = path.display().to_string();
                let item = ListItem::new(SharedString::from(format!(
                    "navigator-worktree-{index}-{row}"
                )))
                .indent_level(1)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(if project.repository_dir.is_some() {
                        IconName::GitBranch
                    } else {
                        IconName::Folder
                    })
                    .color(if selected {
                        Color::Accent
                    } else {
                        Color::Muted
                    }),
                )
                .child(Label::new(label).truncate())
                .end_slot(
                    h_flex()
                        .w(px(48.))
                        .flex_shrink_0()
                        .child(
                            h_flex().w(px(24.)).flex_shrink_0().justify_center().child(
                                IconButton::new(("pin-worktree", row), IconName::Pin)
                                    .when(!pinned, |button| button.visible_on_hover("list_item"))
                                    .icon_size(IconSize::Small)
                                    .icon_color(if pinned { Color::Accent } else { Color::Muted })
                                    .disabled(loading)
                                    .aria_label(if pinned {
                                        "Unpin worktree"
                                    } else {
                                        "Pin worktree"
                                    })
                                    .tooltip(Tooltip::text(if pinned {
                                        "Unpin worktree"
                                    } else {
                                        "Pin worktree"
                                    }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_pin(pin_path.clone(), cx);
                                    })),
                            ),
                        )
                        .child(
                            h_flex()
                                .w(px(24.))
                                .flex_shrink_0()
                                .justify_center()
                                .when(loading, |controls| {
                                    controls.child(
                                        Icon::new(IconName::LoadCircle).with_rotate_animation(2),
                                    )
                                })
                                .when(can_delete && !loading, |controls| {
                                    controls.child(
                                        IconButton::new(("delete-worktree", row), IconName::Trash)
                                            .visible_on_hover("list_item")
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Muted)
                                            .aria_label("Delete worktree")
                                            .tooltip(Tooltip::text("Delete worktree…"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.delete_worktree(
                                                    delete_path.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                }),
                        ),
                )
                .tooltip(Tooltip::text(tooltip))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_path(path.clone(), window, cx)
                }));
                let navigator = cx.entity().downgrade();
                rows.push(
                    right_click_menu(SharedString::from(format!(
                        "navigator-row-menu-{index}-{row}"
                    )))
                    .trigger(move |_, _, _| item)
                    .menu(move |window, cx| {
                        let navigator = navigator.clone();
                        let path = menu_path.clone();
                        ContextMenu::build(window, cx, move |menu, window, _| {
                            let Some(navigator) = navigator.upgrade() else {
                                return menu;
                            };
                            let pin_path = path.clone();
                            let remove_path = path.clone();
                            menu.entry(
                                if pinned {
                                    "Unpin worktree"
                                } else {
                                    "Pin worktree"
                                },
                                None,
                                window.handler_for(&navigator, move |this, _, cx| {
                                    this.toggle_pin(pin_path.clone(), cx)
                                }),
                            )
                            .entry(
                                "Remove from sidebar",
                                None,
                                window.handler_for(&navigator, move |this, _, cx| {
                                    this.remove_worktree_entry(remove_path.clone(), cx)
                                }),
                            )
                            .when(can_delete && !loading, |menu| {
                                menu.separator().entry(
                                    "Delete worktree…",
                                    None,
                                    window.handler_for(&navigator, move |this, window, cx| {
                                        this.delete_worktree(path.clone(), window, cx)
                                    }),
                                )
                            })
                        })
                    })
                    .into_any_element(),
                );
            }
            if let Some(error) = &project.error {
                rows.push(
                    div()
                        .px_3()
                        .py_1()
                        .child(
                            Label::new(error.clone())
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                        .into_any_element(),
                );
            }
        }
        v_flex()
            .id("worktree-navigator")
            .debug_selector(|| "worktree-navigator".into())
            .track_focus(&self.focus_handle)
            .size_full()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .p_2()
                    .justify_between()
                    .child(
                        Label::new("Projects")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("refresh-worktrees", IconName::ArrowCircle)
                            .tooltip(Tooltip::text("Refresh worktrees"))
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.refresh_from_git(window, cx)
                                }),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("navigator-projects")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .when(rows.is_empty(), |list| {
                        list.child(div().p_3().child(
                            Label::new("Add a project to see its worktrees.").color(Color::Muted),
                        ))
                    })
                    .children(rows),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .p_2()
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Button::new("add-navigator-project", "Add project")
                            .full_width()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_project(window, cx)),
                            ),
                    ),
            )
            .when(
                !self.hidden_worktrees.is_empty() || !self.hidden_projects.is_empty(),
                |panel| {
                    panel.child(
                        Button::new("restore-navigator-entries", "Restore removed entries")
                            .full_width()
                            .on_click(
                                cx.listener(|this, _, _, cx| this.restore_removed_entries(cx)),
                            ),
                    )
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs};
    use gpui::{TestAppContext, VisualTestContext};
    use project::{DisableAiSettings, Project};
    use serde_json::json;
    use settings::{Settings, SettingsStore};
    use std::{path::Path, sync::Arc};
    use util::path;

    #[gpui::test]
    async fn restored_multi_folder_layouts_do_not_repeat_repository_entries(
        cx: &mut TestAppContext,
    ) {
        let (fs, multi_workspace, navigator, _workspace_a, _workspace_b, mut cx) = setup(cx).await;
        fs.insert_tree(path!("/scratchpad"), json!({"notes.txt": "notes"}))
            .await;
        multi_workspace.update(&mut cx, |multi_workspace, cx| {
            multi_workspace.restore_project_groups(
                vec![
                    workspace::SerializedProjectGroupState {
                        key: ProjectGroupKey::new(
                            None,
                            PathList::new(&[path!("/project-a"), path!("/scratchpad")]),
                        ),
                        expanded: true,
                    },
                    workspace::SerializedProjectGroupState {
                        key: ProjectGroupKey::new(None, PathList::new(&[path!("/scratchpad")])),
                        expanded: true,
                    },
                ],
                cx,
            );
            cx.emit(MultiWorkspaceEvent::ProjectGroupsChanged);
        });
        cx.run_until_parked();
        navigator.read_with(&cx, |navigator, _| {
            assert_eq!(
                navigator.projects.len(),
                3,
                "two repositories and one scratchpad folder"
            );
            assert_eq!(
                navigator
                    .projects
                    .iter()
                    .filter(|project| project.repository_dir.is_none())
                    .count(),
                1
            );
            assert_eq!(
                navigator
                    .projects
                    .iter()
                    .filter(|project| project_label(project).as_ref() == "project-a")
                    .count(),
                1
            );
        });
    }

    fn worktree_queries(workspace: &Entity<Workspace>, cx: &VisualTestContext) -> usize {
        workspace.read_with(cx, |workspace, cx| {
            let repository = workspace.project().read(cx).active_repository(cx).unwrap();
            let jobs = repository.read(cx).job_debug_queue().to_debug_value();
            jobs["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|job| job["description"] == "worktrees")
                .count()
        })
    }

    async fn create_linked_worktree(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> (Entity<project::git_store::Repository>, PathBuf) {
        let repository = workspace
            .read_with(cx, |workspace, cx| {
                workspace.project().read(cx).active_repository(cx)
            })
            .unwrap();
        let path = PathBuf::from(path!("/linked-a"));
        repository
            .update(cx, |repository, _| {
                repository.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "linked".into(),
                        base_sha: None,
                    },
                    path.clone(),
                )
            })
            .await
            .unwrap()
            .unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        (repository, path)
    }

    #[gpui::test]
    async fn pinned_order_and_removed_entries_survive_sidebar_restoration(cx: &mut TestAppContext) {
        let (fs, multi_workspace, navigator, workspace_a, _workspace_b, mut cx) = setup(cx).await;
        let (_repository, path) = create_linked_worktree(&workspace_a, &mut cx).await;
        navigator.update(&mut cx, |navigator, cx| {
            navigator.toggle_pin(path.clone(), cx);
            let project = navigator
                .projects
                .iter()
                .find(|project| {
                    project
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.path == path)
                })
                .unwrap();
            assert_eq!(navigator.worktree_rows(project).first().unwrap().path, path);
            let id = project.id();
            navigator.remove_worktree_entry(PathBuf::from(path!("/project-a")), cx);
            navigator.remove_project_entry(id, cx);
        });
        let state =
            navigator.read_with(&cx, |navigator, cx| navigator.serialized_state(cx).unwrap());
        let restored = cx.update(|window, cx| {
            cx.new(|cx| WorktreeNavigator::new(multi_workspace.clone(), window, cx))
        });
        restored.update_in(&mut cx, |navigator, window, cx| {
            navigator.restore_serialized_state(&state, window, cx)
        });
        cx.run_until_parked();
        restored.update(&mut cx, |navigator, cx| {
            let project = navigator
                .projects
                .iter()
                .find(|project| {
                    project
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.path == path)
                })
                .unwrap();
            assert!(navigator.hidden_projects.contains(&project.id()));
            let rows = navigator.worktree_rows(project);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows.first().unwrap().path, path);
            navigator.restore_removed_entries(cx);
            let project = navigator
                .projects
                .iter()
                .find(|project| {
                    project
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.path == path)
                })
                .unwrap();
            assert!(!navigator.hidden_projects.contains(&project.id()));
            assert_eq!(navigator.worktree_rows(project).len(), 2);
            assert_eq!(navigator.worktree_rows(project).first().unwrap().path, path);
        });
        multi_workspace.read_with(&cx, |workspace, _| {
            assert_eq!(workspace.workspaces().count(), 2)
        });
        assert!(
            fs.metadata(Path::new(path!("/project-a")))
                .await
                .unwrap()
                .is_some()
        );
        assert!(fs.metadata(&path).await.unwrap().is_some());
    }

    #[gpui::test]
    async fn removed_saved_folder_stays_hidden_when_repository_is_discovered(
        cx: &mut TestAppContext,
    ) {
        let (_fs, _multi_workspace, navigator, _workspace_a, _workspace_b, mut cx) =
            setup(cx).await;
        navigator.update(&mut cx, |navigator, cx| {
            navigator.remove_project_entry(
                NavigatorProjectId::Folders(vec![PathBuf::from(path!("/project-a"))]),
                cx,
            );
            let project = navigator
                .projects
                .iter()
                .find(|project| project_label(project).as_ref() == "project-a")
                .unwrap();
            assert!(navigator.is_project_hidden(project));
        });
        navigator.update_in(&mut cx, |navigator, window, cx| {
            navigator.select_path(PathBuf::from(path!("/project-a")), window, cx)
        });
        cx.run_until_parked();
        navigator.read_with(&cx, |navigator, _| {
            let project = navigator
                .projects
                .iter()
                .find(|project| project_label(project).as_ref() == "project-a")
                .unwrap();
            assert!(
                !navigator.is_project_hidden(project),
                "adding the folder again restores its repository entry"
            );
        });
    }

    #[gpui::test]
    async fn deleting_requires_confirmation_and_cancel_keeps_worktree(cx: &mut TestAppContext) {
        let (fs, _multi_workspace, navigator, workspace_a, _workspace_b, mut cx) = setup(cx).await;
        let (_repository, path) = create_linked_worktree(&workspace_a, &mut cx).await;
        navigator.update_in(&mut cx, |navigator, window, cx| {
            navigator.delete_worktree(path.clone(), window, cx);
            navigator.delete_worktree(path.clone(), window, cx);
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        assert!(fs.metadata(&path).await.unwrap().is_some());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "duplicate clicks must share a confirmation"
        );
        assert!(fs.metadata(&path).await.unwrap().is_some());
        navigator.read_with(&cx, |navigator, _| {
            assert!(navigator.deleting_paths.is_empty())
        });
    }

    #[gpui::test]
    async fn confirmed_deletion_closes_visited_workspace_and_removes_folder(
        cx: &mut TestAppContext,
    ) {
        let (fs, multi_workspace, navigator, workspace_a, _workspace_b, mut cx) = setup(cx).await;
        let (repository, path) = create_linked_worktree(&workspace_a, &mut cx).await;
        navigator.update_in(&mut cx, |navigator, window, cx| {
            navigator.toggle_pin(path.clone(), cx);
            navigator.select_path(path.clone(), window, cx);
        });
        cx.run_until_parked();
        multi_workspace.read_with(&cx, |workspace, _| {
            assert_eq!(workspace.workspaces().count(), 3)
        });
        navigator.update_in(&mut cx, |navigator, window, cx| {
            navigator.delete_worktree(path.clone(), window, cx)
        });
        cx.simulate_prompt_answer("Delete worktree");
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(fs.metadata(&path).await.unwrap().is_none());
        let worktrees = repository
            .update(&mut cx, |repository, _| repository.worktrees())
            .await
            .unwrap()
            .unwrap();
        assert!(!worktrees.iter().any(|worktree| worktree.path == path));
        multi_workspace.read_with(&cx, |workspace, cx| {
            assert_eq!(workspace.workspaces().count(), 2);
            assert!(
                !workspace
                    .workspace()
                    .read(cx)
                    .root_paths(cx)
                    .iter()
                    .any(|root| root.as_ref() == path.as_path())
            );
        });
        navigator.read_with(&cx, |navigator, _| {
            assert!(navigator.error.is_none(), "{:?}", navigator.error);
            assert!(!navigator.pinned_worktrees.contains(&path));
            assert!(navigator.deleting_paths.is_empty());
        });
    }

    #[gpui::test]
    async fn deleting_dirty_worktree_reports_error_and_preserves_files(cx: &mut TestAppContext) {
        let (fs, _multi_workspace, navigator, workspace_a, _workspace_b, mut cx) = setup(cx).await;
        let (_repository, path) = create_linked_worktree(&workspace_a, &mut cx).await;
        fs.with_git_state(Path::new(path!("/project-a/.git")), false, |state| {
            state.worktrees_requiring_force_delete.insert(path.clone());
        })
        .unwrap();
        navigator.update_in(&mut cx, |navigator, window, cx| {
            navigator.delete_worktree(path.clone(), window, cx)
        });
        cx.simulate_prompt_answer("Delete worktree");
        cx.run_until_parked();
        assert!(fs.metadata(&path).await.unwrap().is_some());
        navigator.read_with(&cx, |navigator, _| {
            assert!(
                navigator
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("modified or untracked"))
            );
            assert!(navigator.deleting_paths.is_empty());
            assert!(navigator.projects.iter().any(|project| {
                project
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.path == path)
            }));
        });
    }

    #[gpui::test]
    async fn main_checkout_cannot_be_deleted(cx: &mut TestAppContext) {
        let (fs, _multi_workspace, navigator, _workspace_a, _workspace_b, mut cx) = setup(cx).await;
        navigator.update_in(&mut cx, |navigator, window, cx| {
            navigator.delete_worktree(PathBuf::from(path!("/project-a")), window, cx)
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert!(
            fs.metadata(Path::new(path!("/project-a")))
                .await
                .unwrap()
                .is_some()
        );
        navigator.read_with(&cx, |navigator, _| {
            assert!(
                navigator
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("main checkout is protected"))
            )
        });
    }

    #[gpui::test]
    async fn multiple_workspace_layouts_share_one_repository_entry(cx: &mut TestAppContext) {
        let (fs, multi_workspace, navigator, workspace_a, _workspace_b, mut cx) = setup(cx).await;
        fs.insert_tree(
            path!("/agent-context"),
            json!({"notes.txt": "agent context"}),
        )
        .await;
        let project = Project::test(
            fs,
            [
                Path::new(path!("/project-a")),
                Path::new(path!("/agent-context")),
            ],
            &mut cx,
        )
        .await;
        let additional_workspace = multi_workspace.update_in(&mut cx, |mw, window, cx| {
            mw.test_add_workspace(project, window, cx)
        });
        cx.run_until_parked();
        multi_workspace.read_with(&cx, |mw, _| assert_eq!(mw.workspaces().count(), 3));
        navigator.read_with(&cx, |nav, _| {
            assert_eq!(nav.projects.len(), 2, "one entry per Git repository");
            assert_eq!(
                nav.projects
                    .iter()
                    .flat_map(|project| &project.worktrees)
                    .filter(|worktree| worktree.path == Path::new(path!("/project-a")))
                    .count(),
                1,
                "the same worktree should only appear once"
            );
        });
        let before =
            worktree_queries(&workspace_a, &cx) + worktree_queries(&additional_workspace, &cx);
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.refresh_from_git(window, cx);
        });
        cx.run_until_parked();
        let after =
            worktree_queries(&workspace_a, &cx) + worktree_queries(&additional_workspace, &cx);
        assert_eq!(after, before + 1, "query each shared repository only once");
        navigator.read_with(&cx, |nav, _| assert_eq!(nav.projects.len(), 2));
        multi_workspace.read_with(&cx, |mw, _| assert_eq!(mw.workspaces().count(), 3));
    }

    #[gpui::test]
    async fn separate_clones_with_the_same_name_remain_separate(cx: &mut TestAppContext) {
        let (fs, multi_workspace, navigator, _workspace_a, _workspace_b, mut cx) = setup(cx).await;
        fs.insert_tree(
            path!("/another/project-a"),
            json!({".git": {}, "a.txt": "a different repository"}),
        )
        .await;
        let project = Project::test(fs, [Path::new(path!("/another/project-a"))], &mut cx).await;
        multi_workspace.update_in(&mut cx, |mw, window, cx| {
            mw.test_add_workspace(project, window, cx)
        });
        cx.run_until_parked();
        navigator.read_with(&cx, |nav, _| {
            assert_eq!(nav.projects.len(), 3);
            for path in [path!("/project-a"), path!("/another/project-a")] {
                assert_eq!(
                    nav.projects
                        .iter()
                        .flat_map(|project| &project.worktrees)
                        .filter(|worktree| worktree.path == Path::new(path))
                        .count(),
                    1
                );
            }
        });
    }

    #[gpui::test]
    async fn switching_does_not_enqueue_worktree_queries(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, navigator, workspace_a, workspace_b, mut cx) = setup(cx).await;
        let before = (
            worktree_queries(&workspace_a, &cx),
            worktree_queries(&workspace_b, &cx),
        );
        for _ in 0..10 {
            for path in [path!("/project-a"), path!("/project-b")] {
                navigator.update_in(&mut cx, |nav, window, cx| {
                    nav.select_path(path.into(), window, cx)
                });
                cx.run_until_parked();
            }
        }
        let after = (
            worktree_queries(&workspace_a, &cx),
            worktree_queries(&workspace_b, &cx),
        );
        assert_eq!(after, before, "switching should reuse Git store snapshots");
    }

    #[gpui::test]
    async fn repeated_manual_refresh_is_coalesced(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, navigator, workspace_a, workspace_b, mut cx) = setup(cx).await;
        let before = (
            worktree_queries(&workspace_a, &cx),
            worktree_queries(&workspace_b, &cx),
        );
        navigator.update_in(&mut cx, |nav, window, cx| {
            for _ in 0..10 {
                nav.refresh_from_git(window, cx);
            }
        });
        cx.run_until_parked();
        let after = (
            worktree_queries(&workspace_a, &cx),
            worktree_queries(&workspace_b, &cx),
        );
        assert_eq!(after, (before.0 + 1, before.1 + 1));
    }

    async fn setup(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<MultiWorkspace>,
        Entity<WorktreeNavigator>,
        Entity<Workspace>,
        Entity<Workspace>,
        VisualTestContext,
    ) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.workspace.worktree_navigator = Some(true);
                });
            });
            DisableAiSettings::register(cx);
            DisableAiSettings::override_global(DisableAiSettings { disable_ai: true }, cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project-a"),
            json!({".git": {}, "a.txt": "changed a\n"}),
        )
        .await;
        fs.insert_tree(
            path!("/project-b"),
            json!({".git": {}, "b.txt": "changed b\n"}),
        )
        .await;
        for (root, name) in [
            (path!("/project-a"), "a.txt"),
            (path!("/project-b"), "b.txt"),
        ] {
            fs.set_head_and_index_for_repo(
                &Path::new(root).join(".git"),
                &[(name, "before\n".into())],
            );
            fs.set_status_for_repo(
                &Path::new(root).join(".git"),
                &[(name, git::status::StatusCode::Modified.worktree())],
            );
        }
        let project_a = Project::test(fs.clone(), [Path::new(path!("/project-a"))], cx).await;
        let project_b = Project::test(fs.clone(), [Path::new(path!("/project-b"))], cx).await;
        let handle = cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
        let multi_workspace = handle.entity(cx).unwrap();
        let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_b = handle
            .update(cx, |mw, window, cx| {
                mw.test_add_workspace(project_b, window, cx)
            })
            .unwrap();
        let mut cx = VisualTestContext::from_window(handle.into(), cx);
        let navigator = cx.update(|window, cx| {
            cx.new(|cx| WorktreeNavigator::new(multi_workspace.clone(), window, cx))
        });
        multi_workspace.update(&mut cx, |mw, cx| {
            mw.register_sidebar(navigator.clone(), cx);
            mw.open_sidebar(cx);
        });
        cx.run_until_parked();
        (fs, multi_workspace, navigator, workspace_a, workspace_b, cx)
    }

    #[gpui::test]
    async fn switching_reuses_workspace_and_its_native_diff(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, navigator, workspace_a, workspace_b, mut cx) = setup(cx).await;
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.select_path(path!("/project-a").into(), window, cx)
        });
        cx.run_until_parked();
        let diff_a = workspace_a
            .read_with(&cx, |workspace, cx| {
                workspace.item_of_type::<ProjectDiff>(cx)
            })
            .expect("native diff for A");
        diff_a.read_with(&cx, |diff, cx| {
            assert!(
                diff.excerpt_file_paths(cx)
                    .iter()
                    .any(|path| path.ends_with("a.txt"))
            );
        });
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.select_path(path!("/project-b").into(), window, cx)
        });
        cx.run_until_parked();
        multi_workspace.read_with(&cx, |mw, _| assert_eq!(mw.workspace(), &workspace_b));
        let diff_b = workspace_b
            .read_with(&cx, |workspace, cx| {
                workspace.item_of_type::<ProjectDiff>(cx)
            })
            .expect("native diff for B");
        diff_b.read_with(&cx, |diff, cx| {
            assert!(
                diff.excerpt_file_paths(cx)
                    .iter()
                    .any(|path| path.ends_with("b.txt"))
            );
        });
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.select_path(path!("/project-a").into(), window, cx)
        });
        cx.run_until_parked();
        multi_workspace.read_with(&cx, |mw, _| {
            assert_eq!(mw.workspace(), &workspace_a);
            assert_eq!(mw.workspaces().count(), 2);
        });
        workspace_a.read_with(&cx, |workspace, cx| {
            assert_eq!(workspace.item_of_type::<ProjectDiff>(cx), Some(diff_a))
        });
    }

    #[gpui::test]
    async fn latest_click_wins_and_repeated_clicks_do_not_duplicate_workspaces(
        cx: &mut TestAppContext,
    ) {
        let (_fs, multi_workspace, navigator, _workspace_a, workspace_b, mut cx) = setup(cx).await;
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.select_path(path!("/project-a").into(), window, cx);
            nav.select_path(path!("/project-b").into(), window, cx);
            nav.select_path(path!("/project-b").into(), window, cx);
        });
        cx.run_until_parked();
        multi_workspace.read_with(&cx, |mw, _| {
            assert_eq!(mw.workspace(), &workspace_b);
            assert_eq!(mw.workspaces().count(), 2);
        });
        navigator.read_with(&cx, |nav, _| {
            assert!(nav.loading_paths.is_empty());
            assert!(nav.error.is_none());
        });
    }

    #[gpui::test]
    async fn cold_worktree_load_cannot_override_a_later_selection(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, navigator, workspace_a, _workspace_b, mut cx) = setup(cx).await;
        let repository = workspace_a
            .read_with(&cx, |workspace, cx| {
                workspace.project().read(cx).active_repository(cx)
            })
            .unwrap();
        let linked_path = PathBuf::from(path!("/cold-worktree"));
        repository
            .update(&mut cx, |repository, _| {
                repository.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "cold-worktree".into(),
                        base_sha: None,
                    },
                    linked_path.clone(),
                )
            })
            .await
            .unwrap()
            .unwrap();
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.select_path(linked_path.clone(), window, cx);
            nav.select_path(path!("/project-a").into(), window, cx);
        });
        cx.run_until_parked();
        navigator.read_with(&cx, |nav, _| {
            assert!(
                nav.loading_paths.is_empty(),
                "unfinished loads: {:?}",
                nav.loading_paths
            );
            assert!(nav.error.is_none(), "open error: {:?}", nav.error);
        });
        let loaded = multi_workspace.read_with(&cx, |mw, cx| {
            assert_eq!(mw.workspace(), &workspace_a);
            assert_eq!(mw.workspaces().count(), 3);
            mw.workspace_for_paths(&PathList::new(std::slice::from_ref(&linked_path)), None, cx)
                .expect("cold worktree opened in background")
        });
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.select_path(linked_path, window, cx)
        });
        cx.run_until_parked();
        multi_workspace.read_with(&cx, |mw, cx| {
            assert_eq!(mw.workspace(), &loaded);
            assert_eq!(mw.workspaces().count(), 3);
            assert_eq!(
                loaded.read(cx).project_group_key(cx),
                workspace_a.read(cx).project_group_key(cx)
            );
        });
    }

    #[gpui::test]
    async fn missing_worktree_keeps_current_workspace_and_reports_error(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, navigator, _workspace_a, workspace_b, mut cx) = setup(cx).await;
        navigator.update_in(&mut cx, |nav, window, cx| {
            nav.select_path(path!("/missing").into(), window, cx)
        });
        cx.run_until_parked();
        multi_workspace.read_with(&cx, |mw, _| {
            assert_eq!(mw.workspace(), &workspace_b);
            assert_eq!(mw.workspaces().count(), 2);
        });
        navigator.read_with(&cx, |nav, _| {
            assert!(
                nav.error
                    .as_ref()
                    .is_some_and(|error| error.contains("no longer exists"))
            )
        });
    }

    #[gpui::test]
    async fn discovers_linked_worktrees_without_opening_them(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, navigator, workspace_a, _workspace_b, mut cx) = setup(cx).await;
        let repository = workspace_a
            .read_with(&cx, |workspace, cx| {
                workspace.project().read(cx).active_repository(cx)
            })
            .unwrap();
        let linked_path = PathBuf::from(path!("/linked-a"));
        let create = repository.update(&mut cx, |repository, _| {
            repository.create_worktree(
                git::repository::CreateWorktreeTarget::NewBranch {
                    branch_name: "linked".into(),
                    base_sha: None,
                },
                linked_path.clone(),
            )
        });
        create.await.unwrap().unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        navigator.read_with(&cx, |nav, _| {
            assert!(nav.projects.iter().any(|project| {
                project
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.path == linked_path)
            }))
        });
        multi_workspace.read_with(&cx, |mw, _| assert_eq!(mw.workspaces().count(), 2));
        let remove = repository.update(&mut cx, |repository, _| {
            repository.remove_worktree(linked_path.clone(), true)
        });
        remove.await.unwrap().unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        navigator.read_with(&cx, |nav, _| {
            assert!(!nav.projects.iter().any(|project| {
                project
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.path == linked_path)
            }))
        });
    }
}

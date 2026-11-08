use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use collections::HashSet;
use git::repository::Worktree as GitWorktree;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle, Subscription,
    Task, WeakEntity, Window,
};
use project::git_store::{GitStoreEvent, RepositoryEvent};
use serde::{Deserialize, Serialize};
use settings::{GitDiffBaseSetting, SidebarSide};
use ui::{CommonAnimationExt, ListItem, Tooltip, prelude::*};
use util::{ResultExt, path_list::PathList};
use workspace::{
    MultiWorkspace, MultiWorkspaceEvent, OpenMode, ProjectGroupKey, Sidebar, SidebarEvent,
    Workspace,
};

use crate::{branch_diff::BranchDiff, project_diff::ProjectDiff};

struct ProjectEntry {
    key: ProjectGroupKey,
    worktrees: Vec<GitWorktree>,
    error: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedNavigator {
    width: Option<f32>,
}

pub struct WorktreeNavigator {
    multi_workspace: WeakEntity<MultiWorkspace>,
    focus_handle: FocusHandle,
    width: Pixels,
    scroll: ScrollHandle,
    projects: Vec<ProjectEntry>,
    requested_path: Option<PathBuf>,
    loading_paths: HashSet<PathBuf>,
    error: Option<String>,
    refresh_task: Option<Task<()>>,
    open_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
    repository_subscriptions: Vec<Subscription>,
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
                    MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                    | MultiWorkspaceEvent::WorkspaceAdded(_)
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
            error: None,
            refresh_task: None,
            open_tasks: Vec::new(),
            _subscriptions: subscriptions,
            repository_subscriptions: Vec::new(),
        }
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let groups = multi_workspace.read(cx).project_groups(cx);
        let mut requests = Vec::new();
        self.repository_subscriptions.clear();
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
                self.repository_subscriptions.push(cx.subscribe_in(
                    &git_store,
                    window,
                    |this, _, event, window, cx| {
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
                    },
                ));
            }
            requests.push((
                group.key,
                repository.map(|repository| repository.update(cx, |repo, _| repo.worktrees())),
            ));
        }
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let projects =
                futures::future::join_all(requests.into_iter().map(|(key, request)| async move {
                    let result: Result<Vec<GitWorktree>> = match request {
                        Some(request) => request
                            .await
                            .context("Git worktree request cancelled")
                            .and_then(|result| result),
                        None => Ok(Vec::new()),
                    };
                    match result {
                        Ok(mut worktrees) => {
                            worktrees.retain(|worktree| !worktree.is_bare);
                            worktrees.sort_by(|a, b| {
                                b.is_main.cmp(&a.is_main).then_with(|| a.path.cmp(&b.path))
                            });
                            ProjectEntry {
                                key,
                                worktrees,
                                error: None,
                            }
                        }
                        Err(error) => ProjectEntry {
                            key,
                            worktrees: Vec::new(),
                            error: Some(format!("{error:#}")),
                        },
                    }
                }))
                .await;
            this.update(cx, |this, cx| {
                this.projects = projects;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn select_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.requested_path = Some(path.clone());
        self.error = None;
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

fn project_label(key: &ProjectGroupKey) -> SharedString {
    key.path_list()
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
        })
        .log_err()
    }
    fn restore_serialized_state(&mut self, state: &str, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = serde_json::from_str::<SerializedNavigator>(state).log_err() {
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
            let expanded = multi_workspace
                .as_ref()
                .and_then(|multi_workspace| {
                    multi_workspace.read(cx).group_state_by_key(&project.key)
                })
                .is_none_or(|group| group.expanded);
            let key = project.key.clone();
            rows.push(
                ListItem::new(("navigator-project", index))
                    .start_slot(Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    }))
                    .child(Label::new(project_label(&project.key)).truncate())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.multi_workspace
                            .update(cx, |multi_workspace, cx| {
                                if let Some(group) = multi_workspace.group_state_by_key_mut(&key) {
                                    group.expanded = !group.expanded;
                                }
                                cx.notify();
                            })
                            .log_err();
                        cx.emit(SidebarEvent::SerializeNeeded);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            if !expanded {
                continue;
            }
            let worktrees: Vec<_> = if project.worktrees.is_empty() {
                project
                    .key
                    .path_list()
                    .paths()
                    .iter()
                    .map(|path| (path.clone(), SharedString::from("Open project")))
                    .collect()
            } else {
                project
                    .worktrees
                    .iter()
                    .map(|worktree| (worktree.path.clone(), worktree_label(worktree)))
                    .collect()
            };
            for (row, (path, label)) in worktrees.into_iter().enumerate() {
                let selected = active_path.as_deref() == Some(path.as_path());
                let loading = self.loading_paths.contains(&path);
                let tooltip = path.display().to_string();
                rows.push(
                    ListItem::new(SharedString::from(format!(
                        "navigator-worktree-{index}-{row}"
                    )))
                    .indent_level(1)
                    .toggle_state(selected)
                    .start_slot(Icon::new(IconName::GitBranch).color(if selected {
                        Color::Accent
                    } else {
                        Color::Muted
                    }))
                    .child(Label::new(label).truncate())
                    .when(loading, |item| {
                        item.end_slot(Icon::new(IconName::LoadCircle).with_rotate_animation(2))
                    })
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_path(path.clone(), window, cx)
                    }))
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
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
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
                div()
                    .p_2()
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{DisableAiSettings, Project};
    use serde_json::json;
    use settings::{Settings, SettingsStore};
    use std::{path::Path, sync::Arc};
    use util::path;

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

> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed Worktrees

A Zed fork for reviewing agent work across multiple projects and Git worktrees in one window.
Based on upstream Zed `v1.26.2`.

## What this fork adds

- A project and Git worktree navigator on the right, alongside Zed’s native Changes, History and diff views.
- Worktree switching that keeps each workspace’s terminals and open tabs alive.
- New workspaces that inherit the main checkout’s dock layout and panel sizes.
- Pinned worktrees, compact controls on hover, and confirmed worktree deletion.
- Worktree bell indicators for terminal notifications and completed terminal tasks, with setup instructions for Claude Code and Codex.

## Installation and setup

Build this branch from source using the [installation instructions](docs/src/git.md#worktree-navigator).
There is no published installer for this fork yet.

Enable the navigator in your Zed user settings:

```json
{
  "worktree_navigator": true
}
```

Use **Add project** in the right-hand sidebar to include a repository. Create linked worktrees with
Git or your existing worktree script; the sidebar discovers them automatically.

Completion bells require your agent to emit a terminal notification. See the
[Claude Code and Codex setup](docs/src/git.md#agent-completion-bells).
Terminal processes stay alive while switching workspaces; quitting Zed ends those processes.

## Credits

The editor is [Zed](https://github.com/zed-industries/zed), created by **Zed Industries, Inc. and its contributors**.
This is an unofficial fork maintained by [abboskhon-dev](https://github.com/abboskhon-dev).
The project and worktree workflow was inspired by **herdr**; its code is not included in this fork.

## License

Zed’s existing copyright and license notices are retained. The source is primarily
[GPL-3.0-or-later](LICENSE-GPL), with [Apache-2.0](LICENSE-APACHE) components where marked.
See the original license files and individual component notices for details.

## Upstream development

- [Zed repository](https://github.com/zed-industries/zed)
- [macOS build guide](docs/src/development/macos.md)
- [Linux build guide](docs/src/development/linux.md)
- [Windows build guide](docs/src/development/windows.md)
- [Contributing](CONTRIBUTING.md)

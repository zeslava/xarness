use std::path::Path;

/// `root` shortened to `~/…` under home, plus the git branch if it is inside a repository.
pub fn location(root: &Path) -> String {
    let home = dirs::home_dir();
    let path = match home.as_deref().and_then(|h| root.strip_prefix(h).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Some(rel) => format!("~/{}", rel.display()),
        None => root.display().to_string(),
    };
    match branch(root) {
        Some(b) => format!("{path} · {b}"),
        None => path,
    }
}

/// Read from `HEAD` rather than by running git, so it is cheap enough to refresh every turn.
fn branch(root: &Path) -> Option<String> {
    let dot_git = root
        .ancestors()
        .map(|d| d.join(".git"))
        .find(|p| p.exists())?;
    // Worktrees and submodules have a `.git` file pointing at the real git dir.
    let git_dir = if dot_git.is_file() {
        let s = std::fs::read_to_string(&dot_git).ok()?;
        dot_git.parent()?.join(s.strip_prefix("gitdir:")?.trim())
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: ") {
        Some(r) => r.strip_prefix("refs/heads/").unwrap_or(r).to_string(),
        None => head.get(..7)?.to_string(),
    })
}

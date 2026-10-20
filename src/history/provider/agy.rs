//! Antigravity CLI (agy) sessions: stored under `~/.gemini/antigravity-cli/brain/<uuid>/`.

use super::load::apply_external_title;
use super::storage::SessionStorage;
use super::walk::{self, SessionFiles};
use super::{
    Capability, Deleted, DenialDetail, DiscoveredSessions, RefNamespaces, ResolvedSession,
    RootEnvironment, SessionCache, SessionDeleter, SessionForker, SessionIdResolver, SessionLaunch,
    SessionProvider, SessionRenamer, SessionResumer, SessionRoot, SessionStub, SessionTitle,
    SessionsDirLookup, SourceLabels, SubagentFinder, resolved_session_paths,
};
use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use crate::history::format::SessionFormat;
use crate::history::format::agy::{AGY_TRANSCRIPT, AGY_TRANSCRIPT_FILE};
use crate::history::{Conversation, Source, parser};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct AgyProvider;

/// Custom brain directory override.
const BRAIN_DIR_VARIABLE: &str = "ANTIGRAVITY_BRAIN_DIR";
const DATA_DIR_VARIABLE: &str = "ANTIGRAVITY_DATA_DIR";

impl SessionProvider for AgyProvider {
    fn source(&self) -> Source {
        Source::Agy
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "agy",
            list: "AGY",
            display: "Antigravity",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: "agent-agy-v1",
            project: "agent-agy-project-v1",
        }
    }

    fn storage(&self) -> &dyn SessionStorage {
        &AgyStorage
    }

    fn storage_env_vars(&self) -> &'static [&'static str] {
        &[BRAIN_DIR_VARIABLE, DATA_DIR_VARIABLE]
    }

    fn format(&self) -> &dyn SessionFormat {
        &AGY_TRANSCRIPT
    }

    fn resume(&self) -> Capability<&dyn SessionResumer> {
        Capability::Capable(&AgyLauncher)
    }

    /// agy has no command-line flag that starts a forked session: forking
    /// exists only as the `/fork` command inside a running session, and a
    /// `/fork` passed as the initial prompt reaches the model as plain text.
    ///
    /// Reversing `/opt/homebrew/bin/agy` confirms that `agy` provides no `--fork`
    /// CLI option (`--conversation` only resumes existing sessions). The `/fork`
    /// command is implemented in `third_party/jetski/cli/commands/fork.go` and
    /// `cortex/fork/fork.go` (`CascadeManager.ForkConversation`), running
    /// turn-boundary validation, artifact tree duplication (skipping `.git`, `.jj`,
    /// `.system_generated`, and subagent dirs), text ID rewriting, transcript/media
    /// pruning, and recording `parent_conversation_id` in `conversation_summaries.db`.
    fn fork(&self) -> Capability<&dyn SessionForker> {
        Capability::NotCapable(Some(DenialDetail::ResumeThenForkInside))
    }

    /// agy's `/rename` writes the title to `conversation_summaries.title`.
    ///
    /// Reversing `third_party/jetski/cli/commands/rename.go` and the storage layer
    /// in `jetbox_summaries_store` confirms that session titles reside exclusively
    /// in `conversation_summaries.db` (`conversation_summaries.title`). Neither
    /// `conversations/<id>.db` nor the transcript JSONL records titles.
    fn rename(&self) -> Capability<&dyn SessionRenamer> {
        Capability::Capable(self)
    }

    fn delete(&self) -> Capability<&dyn SessionDeleter> {
        Capability::Capable(self)
    }

    fn subagents(&self) -> Capability<&dyn SubagentFinder> {
        Capability::NotCapable(None)
    }

    fn subagent_by_id(&self) -> Capability {
        Capability::NotCapable(None)
    }

    fn session_id_lookup(&self) -> Capability<&dyn SessionIdResolver> {
        Capability::Capable(self)
    }

    fn session_id_in_locator(&self, locator: &Path) -> Option<String> {
        crate::history::format::agy::session_id_of(locator)
    }

    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        resolved_session_paths(self, session_id)
    }

    fn workspace_sessions_dir(&self) -> Capability<SessionsDirLookup> {
        Capability::NotCapable(None)
    }

    fn current_session_variable(&self) -> Capability<&'static str> {
        Capability::NotCapable(None)
    }
}

/// agy resumes by conversation id (the session directory's name) through
/// `--conversation`, run in the session's working directory.
struct AgyLauncher;

impl SessionResumer for AgyLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<std::process::Command> {
        let id = crate::history::format::agy::session_id_of(launch.path).ok_or_else(|| {
            AppError::ConfigError(format!(
                "{} is not inside an Antigravity session directory",
                launch.path.display()
            ))
        })?;
        let mut command = std::process::Command::new("agy");
        command.arg("--conversation").arg(id);
        if let Some(project_path) = launch.project_path {
            command.current_dir(project_path);
        }
        Ok(command)
    }
}

impl SessionRenamer for AgyProvider {
    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        crate::history::format::require_owned_transcript(Source::Agy, path)?;
        let cleaned = title.replace(['\r', '\n'], " ").trim().to_owned();
        if cleaned.is_empty() {
            return Err(AppError::ConfigError(
                "session title cannot be empty".to_owned(),
            ));
        }
        let id = crate::history::format::agy::session_id_of(path).ok_or_else(|| {
            AppError::ConfigError(format!(
                "{} is not inside an Antigravity session directory",
                path.display()
            ))
        })?;
        let brain = session_dir_of(path)
            .and_then(|dir| dir.parent().map(Path::to_path_buf))
            .ok_or_else(|| AppError::ConfigError("no Antigravity brain directory".to_owned()))?;
        let database = summaries_database(&brain);
        let connection = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| sqlite_error(&database, error))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|error| sqlite_error(&database, error))?;
        let changed = connection
            .execute(
                "UPDATE conversation_summaries SET title = ?1 WHERE conversation_id = ?2",
                (&cleaned, &id),
            )
            .map_err(|error| sqlite_error(&database, error))?;
        if changed == 0 {
            return Err(AppError::ConfigError(format!(
                "{} has no row for conversation {id}",
                database.display()
            )));
        }
        Ok(())
    }
}

fn sqlite_error(database: &Path, error: rusqlite::Error) -> AppError {
    AppError::ConfigError(format!("{}: {error}", database.display()))
}

/// agy keeps every conversation's title in one database beside `brain/`.
fn summaries_database(brain: &Path) -> PathBuf {
    brain
        .parent()
        .unwrap_or(brain)
        .join("conversation_summaries.db")
}

impl SessionDeleter for AgyProvider {
    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        crate::history::format::require_owned_transcript(Source::Agy, path)?;
        if let Some(session_dir) = session_dir_of(path) {
            std::fs::remove_dir_all(&session_dir)?;
            Ok(Deleted::just_the_session())
        } else if path.is_file() {
            std::fs::remove_file(path)?;
            Ok(Deleted::just_the_session())
        } else {
            Err(AppError::ConfigError(format!(
                "{} is not a valid Antigravity session directory",
                path.display()
            )))
        }
    }
}

impl SubagentFinder for AgyProvider {
    fn subagent_transcripts(&self, _locator: &Path) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
}

impl SessionIdResolver for AgyProvider {
    fn is_session_id_shape(&self, query: &str) -> bool {
        crate::search::is_uuid(query)
    }

    fn resolve_session_id(&self, session_id: &str) -> Result<Option<ResolvedSession>> {
        if !self.is_session_id_shape(session_id) {
            return Ok(None);
        }
        for root in AgyStorage.roots()? {
            let session_dir = root.path.join(session_id);
            let transcript = session_dir
                .join(".system_generated")
                .join("logs")
                .join(AGY_TRANSCRIPT_FILE);
            if transcript.is_file()
                && let Some(stub) = walk::session_stubs(
                    &root,
                    vec![SessionFiles {
                        transcript,
                        subagents: Vec::new(),
                    }],
                )
                .into_iter()
                .next()
            {
                return Ok(Some(ResolvedSession { root, stub }));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
fn brain_root_from(override_dir: Option<&str>, home: &Path) -> SessionRoot {
    let base = override_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".gemini/antigravity-cli/brain"));
    SessionRoot::new(base).in_agent_tree()
}

pub struct AgyStorage;

impl SessionStorage for AgyStorage {
    fn source(&self) -> Source {
        Source::Agy
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "agy",
            magic: *b"AGYHST02",
            schema_version: 1,
        }
    }

    fn roots_in(&self, environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        let home = environment.home().ok_or_else(AppError::home_not_found)?;
        let base = if let Some(brain) = environment
            .var(BRAIN_DIR_VARIABLE)
            .filter(|value| !value.is_empty())
        {
            PathBuf::from(brain)
        } else if let Some(data) = environment
            .var(DATA_DIR_VARIABLE)
            .filter(|value| !value.is_empty())
        {
            PathBuf::from(data).join("brain")
        } else {
            home.join(".gemini/antigravity-cli/brain")
        };
        Ok(vec![SessionRoot::new(base).in_agent_tree()])
    }

    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        let mut sessions = Vec::new();
        let entries = match std::fs::read_dir(&root.path) {
            Ok(e) => e,
            Err(_) => return Ok(DiscoveredSessions::complete(Vec::new())),
        };

        for entry in entries.flatten() {
            let session_path = entry.path();
            if !session_path.is_dir() {
                continue;
            }

            let transcript = session_path
                .join(".system_generated")
                .join("logs")
                .join(AGY_TRANSCRIPT_FILE);

            if transcript.is_file() {
                sessions.push(SessionFiles {
                    transcript,
                    subagents: Vec::new(),
                });
            }
        }

        Ok(DiscoveredSessions::complete(walk::session_stubs(
            root, sessions,
        )))
    }

    fn parse_session(
        &self,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Result<Option<Conversation>> {
        let mut conversation =
            parser::process_session_file(stub, &AGY_TRANSCRIPT, debug_level, on_transcript_read)?;
        let Some(conversation) = conversation.as_mut() else {
            return Ok(None);
        };
        // The title and workspace are in conversation_summaries.db beside brain/.
        if let Some(meta) = session_summary_meta(&root.path, &conversation.session_id) {
            if let Some(title) = meta.title {
                apply_external_title(conversation, &SessionTitle::Custom(title));
            }
            if let Some(workspace) = meta.workspace {
                conversation.cwd = Some(workspace.clone());
                conversation.project_path = Some(workspace.clone());
                conversation.project_name =
                    Some(crate::history::format_short_name_from_path(&workspace));
            }
        }
        if conversation
            .project_path
            .as_ref()
            .map_or(true, |p| p.as_os_str().is_empty())
        {
            if let Some(cwd) = conversation.cwd.clone().filter(|p| !p.as_os_str().is_empty()) {
                conversation.project_path = Some(cwd.clone());
                conversation.project_name =
                    Some(crate::history::format_short_name_from_path(&cwd));
            } else {
                conversation.cwd = None;
                conversation.project_path = None;
                conversation.project_name = None;
            }
        }
        Ok(Some(conversation.clone()))
    }

    /// Titles live in `conversation_summaries.title`, agy's own generated
    /// name until `/rename` replaces it. agy does not say which it is, so each
    /// shows as the title agy's own list shows.
    fn external_titles(&self, root: &SessionRoot) -> HashMap<String, SessionTitle> {
        let mut titles = HashMap::new();
        let Ok(connection) = rusqlite::Connection::open_with_flags(
            summaries_database(&root.path),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return titles;
        };
        let Ok(mut statement) = connection
            .prepare("SELECT conversation_id, title FROM conversation_summaries WHERE title != ''")
        else {
            return titles;
        };
        let Ok(rows) = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) else {
            return titles;
        };
        for (id, title) in rows.flatten() {
            titles.insert(id, SessionTitle::Custom(title));
        }
        titles
    }
}

pub(crate) struct AgySummaryMeta {
    pub title: Option<String>,
    pub workspace: Option<PathBuf>,
}

pub(crate) fn parse_workspace_uri(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return None;
    }
    let candidates: Vec<String> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).unwrap_or_default()
    } else {
        vec![trimmed.to_string()]
    };

    for uri in candidates {
        let uri = uri.trim();
        if uri.is_empty() {
            continue;
        }
        let without_scheme = uri.strip_prefix("file://").unwrap_or(uri);
        let decoded = percent_decode(without_scheme);
        let path_str = if cfg!(windows)
            && decoded.starts_with('/')
            && decoded.len() > 3
            && decoded.as_bytes()[1].is_ascii_alphabetic()
            && decoded.as_bytes()[2] == b':'
        {
            &decoded[1..]
        } else {
            &decoded
        };
        if !path_str.is_empty() {
            return Some(PathBuf::from(path_str));
        }
    }
    None
}

pub(crate) fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%' {
            if let (Some(h1), Some(h2)) = (chars.next(), chars.next()) {
                if let Ok(byte) =
                    u8::from_str_radix(std::str::from_utf8(&[h1, h2]).unwrap_or(""), 16)
                {
                    bytes.push(byte);
                    continue;
                }
                bytes.push(b);
                bytes.push(h1);
                bytes.push(h2);
                continue;
            }
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn session_summary_meta(brain: &Path, session_id: &str) -> Option<AgySummaryMeta> {
    let connection = rusqlite::Connection::open_with_flags(
        summaries_database(brain),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    if let Ok(mut statement) = connection.prepare(
        "SELECT title, workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
    ) {
        if let Ok((raw_title, raw_workspaces)) = statement.query_row([session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        }) {
            let title = raw_title.filter(|t| !t.trim().is_empty());
            let workspace = raw_workspaces.and_then(|raw| parse_workspace_uri(&raw));
            return Some(AgySummaryMeta { title, workspace });
        }
    }
    if let Ok(mut statement) = connection
        .prepare("SELECT title FROM conversation_summaries WHERE conversation_id = ?1")
    {
        if let Ok(raw_title) =
            statement.query_row([session_id], |row| row.get::<_, Option<String>>(0))
        {
            let title = raw_title.filter(|t| !t.trim().is_empty());
            return Some(AgySummaryMeta {
                title,
                workspace: None,
            });
        }
    }
    None
}

fn session_dir_of(path: &Path) -> Option<PathBuf> {
    let mut current = path.parent()?;
    if current.file_name() == Some(std::ffi::OsStr::new("logs")) {
        current = current.parent()?;
    }
    if current.file_name() == Some(std::ffi::OsStr::new(".system_generated")) {
        current = current.parent()?;
    }
    Some(current.to_path_buf())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::history::provider::contract_tests::{
        AgentSession, FixtureIds, IdCase, Marker, ProviderFixture,
    };
    use std::ffi::OsString;

    pub(crate) struct AgyFixture;

    fn root_of(home: &Path) -> PathBuf {
        home.join("brain")
    }

    /// The row agy keeps for a conversation in `conversation_summaries.db`,
    /// beside `brain/`.
    pub(crate) fn write_summary_row(brain: &Path, session: &str) {
        write_summary_row_with_workspace(brain, session, "");
    }

    pub(crate) fn write_summary_row_with_workspace(
        brain: &Path,
        session: &str,
        workspace_uris: &str,
    ) {
        let connection = rusqlite::Connection::open(summaries_database(brain)).unwrap();
        connection
            .execute(
                "CREATE TABLE IF NOT EXISTS conversation_summaries \
                 (conversation_id text PRIMARY KEY, title text NOT NULL DEFAULT '', workspace_uris text NOT NULL DEFAULT '')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO conversation_summaries (conversation_id, workspace_uris) \
                 VALUES (?1, ?2) \
                 ON CONFLICT(conversation_id) DO UPDATE SET workspace_uris = excluded.workspace_uris",
                rusqlite::params![session, workspace_uris],
            )
            .unwrap();
    }

    impl ProviderFixture for AgyFixture {
        fn provider(&self) -> &'static dyn SessionProvider {
            Source::Agy.provider()
        }

        fn ids(&self) -> FixtureIds {
            FixtureIds {
                session: "00000000-0000-4000-8000-000000000001",
                other: "00000000-0000-4000-8000-000000000002",
                unknown: "00000000-0000-4000-8000-000000000003",
                session_in_other_case: "00000000-0000-4000-8000-000000000001",
            }
        }

        fn id_case(&self) -> IdCase {
            IdCase::Insensitive
        }

        fn root_under(&self, home: &Path) -> SessionRoot {
            SessionRoot::new(home.join("brain")).in_agent_tree()
        }

        fn write_session(&self, home: &Path, session: &str) -> PathBuf {
            let session_dir = self.root_under(home).path.join(session);
            let logs_dir = session_dir.join(".system_generated").join("logs");
            std::fs::create_dir_all(&logs_dir).unwrap();
            let transcript_path = logs_dir.join(AGY_TRANSCRIPT_FILE);
            let sample_jsonl = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-20T01:42:13Z","content":"<USER_REQUEST>\nPlease inspect the project structure and fix the bug in main.rs so that all tests pass without errors.\n</USER_REQUEST>\nCommand Working Directory: /tmp/test-project"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-20T01:42:14Z","content":"Fixed it successfully. Let me examine the source code first.","thinking":"analyzing bug thoroughly before attempting any changes","tool_calls":[{"name":"read_file","args":{"Path":"/tmp/test-project/src/main.rs"}}],"input_tokens":100,"output_tokens":50}
{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-10-20T01:42:15Z","content":"fn main() {\n    println!(\"Hello, world!\");\n    // file content\n}"}
{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-20T01:42:16Z","content":"Now running tests to observe the bug failure.","tool_calls":[{"name":"run_command","args":{"CommandLine":"cargo test"}}],"input_tokens":120,"output_tokens":40}
{"step_index":4,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-10-20T01:42:17Z","content":"test result: FAILED. 1 failed; 0 passed"}
{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-20T01:42:18Z","content":"Editing the file to resolve the issue.","tool_calls":[{"name":"edit_file","args":{"Path":"/tmp/test-project/src/main.rs"}}],"input_tokens":150,"output_tokens":60}
{"step_index":6,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-10-20T01:42:19Z","content":"replacement complete"}
{"step_index":7,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-20T01:42:20Z","content":"NUMBERED_REPLY_SENTINEL: The bug in main.rs has been identified and fixed, and all unit tests and integration tests now pass successfully."}
"#
            .to_string();
            std::fs::write(&transcript_path, sample_jsonl).unwrap();
            write_summary_row(&root_of(home), session);
            transcript_path
        }

        fn session_working_dir(&self) -> PathBuf {
            PathBuf::from("/tmp/test-project")
        }

        fn root_variables(&self, home: &Path) -> Vec<(&'static str, OsString)> {
            vec![(BRAIN_DIR_VARIABLE, home.join("brain").into())]
        }

        fn roots_from(&self, override_dir: Option<&str>, home: &Path) -> Vec<PathBuf> {
            vec![brain_root_from(override_dir, home).path]
        }

        fn resolve_under(&self, home: &Path, id: &str) -> Result<Option<ResolvedSession>> {
            let root = self.root_under(home);
            let session_dir = root.path.join(id);
            let transcript = session_dir
                .join(".system_generated")
                .join("logs")
                .join(AGY_TRANSCRIPT_FILE);
            if transcript.is_file()
                && let Some(stub) = walk::session_stubs(
                    &root,
                    vec![SessionFiles {
                        transcript,
                        subagents: Vec::new(),
                    }],
                )
                .into_iter()
                .next()
            {
                return Ok(Some(ResolvedSession { root, stub }));
            }
            Ok(None)
        }

        fn foreign_transcript(&self, directory: &Path) -> PathBuf {
            let path = directory.join("foreign.jsonl");
            std::fs::write(&path, b"not an agy transcript").unwrap();
            path
        }

        fn every_record_session(&self) -> AgentSession {
            let temp = tempfile::tempdir().unwrap();
            let path = self.write_session(temp.path(), "00000000-0000-4000-8000-000000000001");
            AgentSession::new(
                Source::Agy,
                path,
                Vec::new(),
                vec![
                    Marker::searched("user text", "inspect the project structure"),
                    Marker::searched("assistant text", "Fixed it successfully."),
                    Marker::shown("thinking", "analyzing bug thoroughly"),
                    Marker::agent_searched("tool call", "/tmp/test-project/src/main.rs")
                        .in_summary_row("Read 1 file"),
                    Marker::searched("tool result", "file content").in_summary_row("Read 1 file"),
                ],
            )
            .with_reply_after_records_without_text("NUMBERED_REPLY_SENTINEL", 8)
            .kept_in(temp)
        }
    }

    const SESSION: &str = "00000000-0000-4000-8000-000000000001";

    #[test]
    fn rename_is_stored_in_agys_summaries_and_listed_as_a_title() {
        let home = tempfile::tempdir().unwrap();
        let transcript = AgyFixture.write_session(home.path(), SESSION);
        assert!(
            AgyStorage
                .external_titles(&AgyFixture.root_under(home.path()))
                .is_empty()
        );

        AgyProvider.rename_session(&transcript, "My title").unwrap();

        let titles = AgyStorage.external_titles(&AgyFixture.root_under(home.path()));
        assert_eq!(
            titles.get(SESSION),
            Some(&SessionTitle::Custom("My title".to_owned()))
        );
    }

    #[test]
    fn rename_fails_without_a_summaries_row_instead_of_inventing_one() {
        let home = tempfile::tempdir().unwrap();
        let transcript = AgyFixture.write_session(home.path(), SESSION);
        let other = AgyFixture.write_session(home.path(), "00000000-0000-4000-8000-000000000002");
        std::fs::remove_file(summaries_database(&home.path().join("brain"))).unwrap();
        write_summary_row(
            &home.path().join("brain"),
            "00000000-0000-4000-8000-000000000009",
        );

        assert!(AgyProvider.rename_session(&transcript, "x").is_err());
        assert!(AgyProvider.rename_session(&other, "x").is_err());
    }

    #[test]
    fn rename_rejects_empty_or_whitespace_title() {
        let home = tempfile::tempdir().unwrap();
        let transcript = AgyFixture.write_session(home.path(), SESSION);

        assert!(AgyProvider.rename_session(&transcript, "").is_err());
        assert!(AgyProvider.rename_session(&transcript, "   \n\t  ").is_err());
    }

    #[test]
    fn data_dir_variable_points_to_brain_subdir() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = home.path().join("custom_data");
        let variable = |name: &str| {
            if name == DATA_DIR_VARIABLE {
                Some(data_dir.as_os_str().to_os_string())
            } else {
                None
            }
        };
        let env = RootEnvironment::new(&variable, Some(home.path().to_path_buf()), None);
        let roots = AgyStorage.roots_in(&env).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].path, data_dir.join("brain"));
    }

    #[test]
    fn fork_is_only_available_inside_a_resumed_session() {
        assert!(matches!(
            AgyProvider.fork(),
            Capability::NotCapable(Some(DenialDetail::ResumeThenForkInside))
        ));
    }

    #[test]
    fn resume_runs_agy_with_the_conversation_id_in_the_session_directory() {
        let home = tempfile::tempdir().unwrap();
        let transcript = AgyFixture.write_session(home.path(), SESSION);
        let project = PathBuf::from("/tmp/test-project");
        let launch = SessionLaunch {
            path: &transcript,
            project_path: Some(&project),
            configured_args: &[],
        };

        let command = AgyLauncher.resume_command(&launch).unwrap();

        assert_eq!(command.get_program(), "agy");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--conversation", SESSION]);
        assert_eq!(command.get_current_dir(), Some(project.as_path()));
    }

    #[test]
    fn resume_refuses_a_path_outside_a_session_directory() {
        let launch = SessionLaunch {
            path: Path::new("/"),
            project_path: None,
            configured_args: &[],
        };
        assert!(AgyLauncher.resume_command(&launch).is_err());
    }

    #[test]
    fn parse_workspace_uri_extracts_paths_and_handles_percent_encoding() {
        assert_eq!(
            parse_workspace_uri(r#"["file:///Users/naokiiida/Documents/4_clone/rearview"]"#),
            Some(PathBuf::from("/Users/naokiiida/Documents/4_clone/rearview"))
        );
        assert_eq!(
            parse_workspace_uri(r#"["file:///Users/naokiiida/Documents/Vault/Projects/Daily%20Digest"]"#),
            Some(PathBuf::from("/Users/naokiiida/Documents/Vault/Projects/Daily Digest"))
        );
        assert_eq!(
            parse_workspace_uri("file:///tmp/simple-path"),
            Some(PathBuf::from("/tmp/simple-path"))
        );
        assert_eq!(parse_workspace_uri("[]"), None);
        assert_eq!(parse_workspace_uri(""), None);
    }

    #[test]
    fn percent_decode_decodes_special_characters_and_multibyte() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("/path%2Fsub%20dir"), "/path/sub dir");
        assert_eq!(percent_decode("%E9%A1%B9%E7%9B%AE"), "项目");
    }

    #[test]
    fn parse_session_reads_workspace_from_summaries_database() {
        let home = tempfile::tempdir().unwrap();
        let transcript = AgyFixture.write_session(home.path(), SESSION);
        write_summary_row_with_workspace(
            &root_of(home.path()),
            SESSION,
            r#"["file:///workspace/my-custom-project"]"#,
        );

        let root = AgyFixture.root_under(home.path());
        let stub = SessionStub {
            locator: transcript,
            subagents: Vec::new(),
            cache_key: String::new(),
            fingerprint: crate::history::provider::Fingerprint {
                size: 0,
                modified: None,
            },
        };

        let conversation = AgyStorage
            .parse_session(&stub, &root, None, &|| {})
            .unwrap()
            .unwrap();

        assert_eq!(
            conversation.project_path,
            Some(PathBuf::from("/workspace/my-custom-project"))
        );
        assert_eq!(
            conversation.project_name,
            Some("my-custom-project".to_string())
        );
    }
}

//! Antigravity CLI (agy) sessions: stored under `~/.gemini/antigravity-cli/brain/<uuid>/`.

use super::storage::SessionStorage;
use super::walk::{self, SessionFiles};
use super::{
    Capability, Deleted, DiscoveredSessions, RefNamespaces, ResolvedSession, RootEnvironment,
    SessionCache, SessionDeleter, SessionForker, SessionIdResolver, SessionProvider,
    SessionRenamer, SessionResumer, SessionRoot, SessionStub, SessionTitle, SessionsDirLookup,
    SourceLabels, SubagentFinder, resolved_session_paths,
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
        Capability::NotCapable(None)
    }

    fn fork(&self) -> Capability<&dyn SessionForker> {
        Capability::NotCapable(None)
    }

    fn rename(&self) -> Capability<&dyn SessionRenamer> {
        Capability::NotCapable(None)
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
            magic: *b"AGYHST01",
            schema_version: 1,
        }
    }

    fn roots_in(&self, environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        let brain_override = environment
            .var(BRAIN_DIR_VARIABLE)
            .or_else(|| environment.var(DATA_DIR_VARIABLE));
        let home = environment.home().ok_or_else(AppError::home_not_found)?;
        Ok(vec![brain_root_from(brain_override.as_deref(), home)])
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
        _root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(stub, &AGY_TRANSCRIPT, debug_level, on_transcript_read)
    }

    fn external_titles(&self, _root: &SessionRoot) -> HashMap<String, SessionTitle> {
        HashMap::new()
    }
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
}

# Antigravity CLI releases

Steps: [provider release review](../provider-releases.md).

## Listing

Antigravity CLI (`agy`) is distributed through internal or packaged channels.
Installed binary version can be checked via `agy --version`.

Sessions are stored in `~/.gemini/antigravity-cli/brain/`.

## Source

Internal tooling for Google Antigravity. Transcripts are stored in JSONL format under `.system_generated/logs/transcript.jsonl` (and `transcript_full.jsonl`).
Session titles and workspace paths are tracked in `conversation_summaries.db`, and model metadata in `conversations/<id>.db`.

## Schema version

The database `conversation_summaries.db` tracks migrations with SQLite `PRAGMA user_version`.
The reader pins `user_version` to 3 (`NEWEST_VERIFIED_USER_VERSION` in `src/history/provider/agy.rs`).

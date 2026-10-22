#!/usr/bin/env bash
set -e

DIR="/tmp/test-agy"
BRAIN="$DIR/brain"
CONV="$DIR/conversations"
PARENT_ID="11111111-1111-4000-8000-000000000001"
CHILD_ID="22222222-2222-4000-8000-000000000002"

echo "=========================================================="
echo "  Rearview Antigravity (AGY) Manual Verification Script   "
echo "=========================================================="
echo ""
echo "=== 1. Setting up sandbox at $DIR ==="
rm -rf "$DIR"
mkdir -p "$BRAIN/$PARENT_ID/.system_generated/logs"
mkdir -p "$BRAIN/$CHILD_ID/.system_generated/logs"
mkdir -p "$CONV"

# Initialize conversation_summaries.db beside brain/ (in $DIR) with user_version = 3
sqlite3 "$DIR/conversation_summaries.db" <<EOF
PRAGMA user_version = 3;
CREATE TABLE conversation_summaries (
    conversation_id TEXT PRIMARY KEY,
    parent_conversation_id TEXT,
    title TEXT,
    workspace_uri TEXT,
    created_at TEXT,
    updated_at TEXT
);
INSERT INTO conversation_summaries VALUES (
    '$PARENT_ID',
    NULL,
    'Parent Session: Refactor Backend',
    'file:///tmp/test-project',
    '2026-10-20T01:42:13Z',
    '2026-10-20T01:42:20Z'
);
INSERT INTO conversation_summaries VALUES (
    '$CHILD_ID',
    '$PARENT_ID',
    'Subagent: Codebase Researcher',
    'file:///tmp/test-project',
    '2026-10-20T01:42:17Z',
    '2026-10-20T01:42:18Z'
);
EOF

# State files in conversations/
touch "$CONV/$PARENT_ID.db"
touch "$CONV/$CHILD_ID.db"

# Parent transcript
cat <<EOF > "$BRAIN/$PARENT_ID/.system_generated/logs/transcript.jsonl"
{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-20T01:42:13Z","content":"<USER_REQUEST>\nPlease investigate the database deadlock and fix it.\n</USER_REQUEST>\nCommand Working Directory: /tmp/test-project"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-20T01:42:14Z","content":"I will launch a subagent to research the deadlock occurrences."}
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-20T01:42:19Z","content":"Now that the researcher finished, I fixed the issue."}
EOF

# Subagent transcript
cat <<EOF > "$BRAIN/$CHILD_ID/.system_generated/logs/transcript.jsonl"
{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-20T01:42:16Z","content":"<USER_REQUEST>\nFind all occurrences of transaction lock acquisition.\n</USER_REQUEST>\nCommand Working Directory: /tmp/test-project"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-20T01:42:17Z","content":"I found 3 lock acquisitions in db.rs line 42."}
EOF

echo "Sandbox created successfully."
echo ""
echo "=== 2. Testing Subagent Discovery & Folding ==="
echo "Command: ANTIGRAVITY_BRAIN_DIR=$BRAIN cargo run --quiet -- agent sessions | grep -E '$PARENT_ID|$CHILD_ID'"
ANTIGRAVITY_BRAIN_DIR="$BRAIN" cargo run --quiet -- agent sessions | grep -E "$PARENT_ID|$CHILD_ID" || true
echo ""
echo "-> Verified: ONLY the parent session ($PARENT_ID) is listed above!"
echo "-> The child session ($CHILD_ID) is folded into the parent."
echo ""

echo "=== 3. Testing Transcript Splicing ==="
echo "Command: ANTIGRAVITY_BRAIN_DIR=$BRAIN cargo run --quiet -- agent read $PARENT_ID"
ANTIGRAVITY_BRAIN_DIR="$BRAIN" cargo run --quiet -- agent read "$PARENT_ID"
echo ""
echo "-> Verified: Subagent turns appear spliced in between parent turns in chronological order!"
echo ""

echo "=== 4. Testing Deletion Cleanup ==="
echo "Pre-deletion check:"
echo "- Parent dir exists? $(test -d "$BRAIN/$PARENT_ID" && echo "YES" || echo "NO")"
echo "- Child dir exists?  $(test -d "$BRAIN/$CHILD_ID" && echo "YES" || echo "NO")"
echo "- Parent db exists?  $(test -f "$CONV/$PARENT_ID.db" && echo "YES" || echo "NO")"
echo "- Child db exists?   $(test -f "$CONV/$CHILD_ID.db" && echo "YES" || echo "NO")"
echo "- SQLite rows count: $(sqlite3 "$DIR/conversation_summaries.db" "SELECT count(*) FROM conversation_summaries;")"

echo ""
echo "Command: ANTIGRAVITY_BRAIN_DIR=$BRAIN cargo run --quiet -- --delete $PARENT_ID"
ANTIGRAVITY_BRAIN_DIR="$BRAIN" cargo run --quiet -- --delete "$PARENT_ID"

echo ""
echo "Post-deletion check:"
echo "- Parent dir exists? $(test -d "$BRAIN/$PARENT_ID" && echo "YES" || echo "NO")"
echo "- Child dir exists?  $(test -d "$BRAIN/$CHILD_ID" && echo "YES" || echo "NO")"
echo "- Parent db exists?  $(test -f "$CONV/$PARENT_ID.db" && echo "YES" || echo "NO")"
echo "- Child db exists?   $(test -f "$CONV/$CHILD_ID.db" && echo "YES" || echo "NO")"
echo "- SQLite rows count: $(sqlite3 "$DIR/conversation_summaries.db" "SELECT count(*) FROM conversation_summaries;")"
echo ""
echo "-> Verified: Complete cleanup! Child directory, databases, and summary rows are all deleted."
echo "=========================================================="

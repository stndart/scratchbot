# Telegram cleanup history

The bot retains every recorded delivery outcome and Telegram message timestamp in
`STATE_DIR/delivered.sqlite3`. Clearing a Telegram copy marks it removed; it never
erases its outcome or timestamp. A removed failure stops blocking earlier saved
forwards, while remaining available in the audit history.

On first startup the database imports `delivered.json` transactionally. The JSON
file is left intact for rollback. Dates the previous implementation already
forgot cannot be reconstructed reliably; reports identify missing dates rather
than deriving them from message IDs, forwarding origins, or ingestion times.
When a matching Telegram reply object supplies a legacy timestamp, it is saved.

`/show-sent` considers only the requesting user's messages in this chat, before
the command. It validates failures from newest to oldest until one remains, then
looks for the earliest remaining successful copy after that failure:

- If a successful copy remains, the report starts there and replies to it. It
  explains the 48-hour deletion limit, or the deletion refusal recorded by
  `/clear`.
- If none remain, the report uses the earliest saved message after that failure,
  including removed successes, and replies to the failure. Removing the failure
  for both participants allows the next call to walk further back.
- With no remaining failure, the same logic covers the entire recorded history.
  A failure without later successes and an empty history have separate messages.

Permission to edit and cached reply metadata are not existence checks. Validation
forwards a silent temporary copy in the same chat and immediately deletes that
copy. It never deletes the source. Protected content, API failures, rate limits,
and failed probe cleanup do not cause source records to be marked removed.
Dates remain useful if Telegram omits the reply shortcut. A source disappearing
between validation and sending causes bounded reselection.

Telegram bot history and the user's visible history can differ after deletion
for only one participant. A successful source forwarding operation establishes
availability to the bot, not visibility in the user's client. The Bot API does
not expose the latter. All presence decisions refer to the bot's history.

Cleanup commands wait for pending albums, and history queries stop at the command
message ID. Repeated getUpdates results do not extend an album's grace period.
Unsupported user media is recorded as a failure instead of being silently skipped.

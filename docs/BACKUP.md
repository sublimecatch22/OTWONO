# Backup and restore

Everything OTWONO knows is in one folder. Back that folder up and you have
backed up everything; copy it to another machine and OTWONO carries on there.

| Platform | Data directory |
|---|---|
| Windows | `%APPDATA%\OTWONO AI` |
| macOS | `~/Library/Application Support/OTWONO AI` |
| Linux | `~/.local/share/otwono-ai` |

The exact path for your installation is on the Settings screen under
**Your data**.

## What is in it

| | |
|---|---|
| `otwono.sqlite3` | The database: settings, agents, conversations, knowledge index, projects, permissions, the activity log. |
| `backups/` | Copies taken before schema changes, and by *Back up now*. |
| `attachments/` | Files you attached to conversations, copied in so history stays stable. |
| `projects/<id>/` | What projects produced. |
| `runtime.json` | The current session's handshake. Recreated every start; not worth backing up. |
| The vault files | Only on systems without an OS credential store. |

## What is *not* in it

**Your own documents.** OTWONO indexes the folders you authorise; it does not
copy them. Backing up OTWONO does not back up your files, and restoring OTWONO
onto a machine that does not have those folders leaves the sources listed but
marked as missing. Re-index once the folders are back.

**Credentials, on most systems.** API keys and relay tokens are in the
operating system's credential store, which is not inside the data directory. A
restored installation will ask for them again. Only where there is no OS vault
do they live (encrypted) in the data directory.

## Taking a backup

**From the application.** Settings → *Your data* → **Back up now**. This uses
SQLite's backup API, so it is consistent and safe while OTWONO is running. The
message tells you where it went.

**By hand.** Close OTWONO, then copy the whole data directory. Copying it while
OTWONO is running can catch a write in progress; use *Back up now* instead.

**On a schedule.** Point your normal backup tool at the data directory. If it
runs while OTWONO is open, include `otwono.sqlite3-wal` and
`otwono.sqlite3-shm` as well, or take a copy from `backups/` instead, which is
always consistent.

## Restoring

1. Close OTWONO.
2. Copy the backup over `<data directory>/otwono.sqlite3`.
3. Delete `otwono.sqlite3-wal` and `otwono.sqlite3-shm` if they are there.
4. Start OTWONO.
5. Re-enter any API keys, and check Knowledge for sources marked as missing.

Restoring a backup from an **older** version is fine: migrations run forward,
and a fresh backup is taken first. Restoring one from a **newer** version is
refused rather than downgraded.

## Moving to another machine

1. Close OTWONO on both.
2. Copy the data directory across.
3. Install OTWONO on the new machine and start it.
4. Re-enter API keys — they were in the old machine's credential store.
5. Re-authorise knowledge folders if their paths are different, and index again.

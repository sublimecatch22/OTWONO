# Upgrading

An upgrade keeps everything: your settings, conversations, agents, projects,
knowledge index and connections. That claim is tested, not assumed — see
`e2e/06-upgrade-preserves-data.spec.ts`, which restarts the service over a data
directory it has already filled and checks each of those things is still there.

---

## How it works

1. Install the new version over the old one. No need to uninstall first.
2. On the first start, OTWONO reads the database's schema version.
3. If the new version has migrations to apply:
   - **It takes a backup first**, through SQLite's own backup API, into
     `<data directory>/backups/`, named for the version it was taken at.
   - It applies the migrations **in a transaction**. Either all of them land or
     none do.
4. If the database was written by a **newer** version than the one you are
   running, OTWONO stops and says so rather than dropping columns it does not
   understand. Install the newer version again, or restore a backup from before
   the upgrade.

## Before a major upgrade

Not required, but cheap:

Settings → **Your data** → **Back up now**, and note where it saved.

## After upgrading

Look at Settings → *Your data*. The **database version** should have moved and
the **data folder** should be unchanged. If chat had a connection before, it
still does.

## Rolling back

1. Close OTWONO.
2. Copy the backup you want over `<data directory>/otwono.sqlite3`.
3. Install the older version.
4. Start it.

Because a newer schema is refused rather than downgraded, rolling back the
application without also restoring the database will stop with a clear message
instead of damaging your data.

## Reinstalling from scratch

To start completely fresh, delete the data directory. Everything OTWONO knows
is in it, so this is a real reset, not a partial one.

| Platform | Path |
|---|---|
| Windows | `%APPDATA%\OTWONO AI` |
| macOS | `~/Library/Application Support/OTWONO AI` |
| Linux | `~/.local/share/otwono-ai` |

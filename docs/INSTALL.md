# Installing OTWONO AI

OTWONO runs on your own machine and needs no account. It will start and be
useful before you connect a model — you can create agents, organise projects
and index folders — but chat needs a model, so most people connect one first.

---

## Before you start

**A model runtime.** OTWONO does not ship a model. Install one of:

| | |
|---|---|
| **[Ollama](https://ollama.com)** | The simplest. Install it, then `ollama pull llama3.1` in a terminal. |
| **[LM Studio](https://lmstudio.ai)** | A graphical alternative. Download a model, then start its local server. |

Anything with an OpenAI-compatible endpoint — llama.cpp's server, vLLM, LocalAI
— also works; you add it by hand.

**For search that understands meaning**, also pull an embedding model:

```bash
ollama pull nomic-embed-text
```

Without one, search still works, matching words rather than meaning — and the
Knowledge screen tells you that is what is happening.

**Disk space.** OTWONO itself is modest. Models are not: a small one is about
5 GB.

---

## Windows

1. Download `OTWONO AI_<version>_x64-setup.exe` (or the `.msi`).
2. Run it. **These builds are unsigned**, so Windows SmartScreen will warn about
   an unknown publisher. Choose *More info* → *Run anyway* if you trust where
   you got the file. Check the SHA-256 against the published checksum first:

   ```powershell
   Get-FileHash -Algorithm SHA256 .\OTWONO AI_0.1.0_x64-setup.exe
   ```

3. The installer offers a Start Menu entry and, optionally, a desktop shortcut.
4. Launching at sign-in is **off** by default. Turn it on in Settings if you
   want it; nothing is added to your startup without being asked.

Your data lives in `%APPDATA%\OTWONO AI`.

## macOS

1. Open the `.dmg` and drag OTWONO AI to Applications.
2. Unsigned builds are refused on first launch. Right-click the application and
   choose *Open*, then confirm.

Your data lives in `~/Library/Application Support/OTWONO AI`.

## Linux (Debian, Ubuntu)

```bash
sudo dpkg -i otwono-ai_0.1.0_amd64.deb
sudo apt-get install -f      # if anything was missing
```

Your data lives in `~/.local/share/otwono-ai`.

## From source

```bash
git clone https://github.com/sublimecatch22/otwono.git
cd otwono
npm install
npm run desktop:build        # a package for this platform
```

Needs Node.js 20+, Rust (stable), and — on Linux — the WebKitGTK development
packages Tauri lists for your distribution.

---

## First run

1. **Connections** → *Find local runtimes*. Ollama and LM Studio are found on
   their usual ports. If yours is somewhere else, use *Add a connection by hand*
   and say which runtime is listening.
2. Press **Test**. You will see the models it serves, and for each one whether
   its capabilities were **reported** by the runtime, **probed**, or **guessed
   from the name**.
3. Choose a **default model**, and an **embedding model** if you have one.
4. Tick **Use this connection**.
5. Go to **Chat** and say something.

## Verifying a download

Every release folder has a `SHA256SUMS` file.

```bash
sha256sum -c SHA256SUMS       # Linux, macOS
```

```powershell
Get-FileHash -Algorithm SHA256 .\<file>   # Windows
```

## Uninstalling

| | |
|---|---|
| **Windows** | Settings → Apps → OTWONO AI → Uninstall. |
| **macOS** | Drag it to the Bin. |
| **Linux** | `sudo dpkg -r otwono-ai` |

**Uninstalling leaves your data.** The database, backups and artefacts stay in
the data directory so that reinstalling picks up where you left off. To remove
everything, delete that folder yourself — the path is on the Settings screen
under *Your data*.

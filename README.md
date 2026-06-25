# todui

`todui` is a terminal TODO app backed by local JSON project files.

## Install

Download the archive for your system from GitHub Releases, extract it, and put
the `todui` binary somewhere on your `PATH`.

Linux example:

```sh
tar -xzf todui-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
mkdir -p ~/.local/bin
install -m 755 todui ~/.local/bin/todui
```

macOS example:

```sh
tar -xzf todui-v0.1.0-aarch64-apple-darwin.tar.gz
chmod +x todui
mv todui /usr/local/bin/todui
```

The macOS binaries are unsigned. If macOS blocks the downloaded binary, remove
the quarantine attribute:

```sh
xattr -dr com.apple.quarantine /usr/local/bin/todui
```

## Data Location

On first run, `todui` creates its config directory and copies default files
there.

- Linux: `$XDG_CONFIG_HOME/todui`, or `~/.config/todui`
- macOS: `~/Library/Application Support/todui`

The runtime files live there:

- `settings.json`
- `data/*.json`
- `data/INSTRUCTIONS.md`
- `themes/*.tmTheme`
- `schemas/chat-bar-todo.schema.json` (managed by the app)

Existing repo-local TODO files are not copied automatically. To migrate them on
Linux:

```sh
mkdir -p ~/.config/todui/data
cp data/*.json ~/.config/todui/data/
```

On macOS, copy them to:

```sh
mkdir -p "$HOME/Library/Application Support/todui/data"
cp data/*.json "$HOME/Library/Application Support/todui/data/"
```

## AI Drafts

The chat bar uses `codex exec` by default, so Codex must be installed and
authenticated for AI drafting. To use an OpenAI-compatible API instead, switch
the LLM backend in settings and set:

```sh
export TODUI_LLM_API_KEY=...
```

## Release

Releases are built from version tags:

```sh
git tag v0.1.0
git push origin v0.1.0
```

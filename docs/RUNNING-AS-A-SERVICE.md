# Running As A Service

How to keep `memoryd serve` running under a supervisor. The daemon binds
`127.0.0.1:7077` by default, logs structured lines to stderr, stores data at
`~/.local/share/memoryd/memoryd.db` (XDG-aware), and shuts down gracefully on
SIGTERM/SIGINT (drains workers, ~5s) — so plain `SIGTERM`-based supervision
is safe. See `docs/OPERATIONS.md` for backup and file hygiene.

## systemd (Linux, user unit)

Write `~/.config/systemd/user/memoryd.service`:

```ini
[Unit]
Description=memoryd local memory daemon
After=default.target

[Service]
ExecStart=%h/.local/bin/memoryd serve
Restart=on-failure
# Optional: provider settings. `memoryd setup` generates
# ~/.config/memoryd/env (mode 0600) in exactly this format.
EnvironmentFile=-%h/.config/memoryd/env
# Or inline single variables instead:
# Environment=MEMORYD_RETAIN_RAW_DAYS=180

[Install]
WantedBy=default.target
```

Adjust `ExecStart` to wherever the binary is installed. Then:

```bash
systemctl --user daemon-reload
systemctl --user enable --now memoryd
```

stderr goes to the journal:

```bash
journalctl --user -u memoryd -f
```

User units normally stop when your session ends. To keep memoryd running
without an active login session, enable lingering once:

```bash
loginctl enable-linger "$USER"
```

## launchd (macOS)

Write `~/Library/LaunchAgents/com.memoryd.serve.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.memoryd.serve</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/memoryd</string>
    <string>serve</string>
  </array>
  <key>KeepAlive</key>
  <true/>
  <key>StandardErrorPath</key>
  <string>/tmp/memoryd.stderr.log</string>
</dict>
</plist>
```

Load it (and start immediately):

```bash
launchctl load ~/Library/LaunchAgents/com.memoryd.serve.plist
```

`launchctl unload` stops it; launchd sends SIGTERM, which memoryd handles
gracefully. To pass environment variables (e.g. the values from
`~/.config/memoryd/env`), add an `EnvironmentVariables` dict to the plist —
launchd has no env-file equivalent.

## Verifying

```bash
curl -sS http://127.0.0.1:7077/v1/health
memoryd doctor
```

Health returns `{"status":"ok",...}`; `doctor` checks schema, integrity, and
free disk, and exits non-zero on failure.

# gse-engine

Core emulator patching engine for `drop-gse` (spec:
[`docs/architecture/SPECIFICATION.md`](../../docs/architecture/SPECIFICATION.md) §3.1).

## Modules

| Module | Responsibility |
| --- | --- |
| `anticheat` | Fail-closed pre-flight gate — refuses to patch when EAC/BattlEye markers are present or the scan itself fails |
| `dll` | Hash-manifested backup (`<name>.orig` + `.drop-gse-manifest.json`), replace, verified restore |
| `interfaces` | SteamRoll-style extraction of Steam interface version strings from the ORIGINAL binary (64 KiB chunks, 1 KiB overlap, sorted+deduped) |
| `fetch` | Pinned-release download (`releases/download/<tag>/<asset>`, never "latest") with SHA-256 verification and digest-keyed cache; extracts `release/regular/<arch>/<lib>` from gbe_fork archives |
| `config` | Atomic, idempotent `steam_settings/` writer (`steam_appid.txt`, `custom_broadcasts.txt`, `steam_interfaces.txt`, minimal `configs.main.ini`) and teardown that only removes engine-owned files |
| `patch` | Orchestration: anticheat gate → locate targets → extract interfaces from original → backup → replace → configure. Idempotent re-runs preserve verified originals (`BackupState::AlreadyPatched`) |

## CLI

```sh
gse-engine patch --game-dir ./game --app-id 480 \
  --broadcast 100.64.0.2 --broadcast 100.64.0.3 \
  --tag v0.9.9 --asset emu-win-release.7z --digest <sha256>
# prints a JSON PatchReport {targets: {…}, interfaces: […]}

gse-engine restore --game-dir ./game

gse-engine interfaces ./game/steam_api64.dll   # debug helper
```

The library API is async (tokio) for embedding in Drop's Tauri client; the CLI
wraps it with a current-thread runtime.

## Testing

33 tests cover: interface extraction incl. chunk-boundary overlap, digest
verification/cache behavior, config idempotency + user-content safety,
backup/restore roundtrips, tamper/stale-backup rejection, anticheat abort
before any mutation, and full patch→restore roundtrips against fixture game
dirs (no network required — downloads are injectable).

Emulator binaries are never committed; they are fetched pinned+verified at
runtime per spec §5.

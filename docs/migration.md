# План переноса ha_cli: Python → Rust

## Цель

Перенос `../nanobot_workspace/ha_cli` (Python 3.11+, httpx) в отдельный
репозиторий на Rust. Итоговый артефакт — единый статический бинарник `ha`
без зависимости от Python в рантайме, пригодный для деплоя на хост
Home Assistant и в образы nanobot.

## Исходное состояние

Python-пакет из 9 модулей (`ha_cli/`), ~750 строк кода, 8 тест-файлов
(pytest). Внешняя зависимость одна — httpx. Протокол общения с HA —
MCP over HTTP JSON-RPC (`/api/mcp/assist`, protocolVersion `2025-03-26`).

## Соответствие модулей

| Python                | Rust                  | Назначение                                          |
|-----------------------|-----------------------|-----------------------------------------------------|
| `errors.py`           | `src/errors.rs`       | Типы ошибок, exit-коды 0–10, JSON-вывод ошибки      |
| `security.py`         | `src/security.rs`     | Redact секретов, права 0600, запрет entity_id       |
| `config.py`           | `src/config.rs`       | Конфиг TOML + env (`HA_URL`, `HA_TOKEN`, …)         |
| `client.py`           | `src/client.rs`       | MCP JSON-RPC клиент поверх HTTP                     |
| `models.py`           | `src/models.rs`       | `Entity`, парсинг raw-описаний                      |
| `discovery.py`        | `src/discovery.rs`    | Кэш tools (TTL 300 c), basename-маппинг            |
| `intents.py`          | `src/intents.rs`      | Белый список интентов, нормализация аргументов      |
| `context.py`          | `src/context.rs`      | Live Context, build/compact, query_state            |
| `cli.py`              | `src/cli.rs`          | clap-парсер: `context` / `tools` / `intent`         |

## Выбор зависимостей

| Задача           | Крейт                          | Обоснование                                   |
|------------------|--------------------------------|-----------------------------------------------|
| CLI              | `clap` (derive)                | Стандарт; заменяет argparse                   |
| HTTP             | `reqwest` blocking + rustls    | Без OpenSSL; аналог httpx sync-клиента        |
| JSON             | `serde` / `serde_json`         | Стандарт                                      |
| TOML-конфиг      | `toml`                         | Замена stdlib `tomllib`                       |
| Пути (~, XDG)    | `dirs`                         | Замена `os.path.expanduser`                   |
| Тесты            | встроенный `cargo test`        | Замена pytest                                 |

Тестовый транспорт: трейт `Transport` в `client.rs` — аналог инъекции
`httpx.BaseTransport` в Python-тестах, моки без сети.

## Контракт совместимости (нельзя ломать)

1. CLI-интерфейс: `ha [--debug] {context [--raw|--compact], tools [--json|--refresh], intent NAME [PAYLOAD]}`.
2. Exit-коды 0–10 и формат ошибки `{"ok": false, "error": {type, message}}` в stderr.
3. Формат успешного ответа: одна строка JSON (`ensure_ascii=false` → без escape не-ASCII) в stdout.
4. Приоритет конфигурации: CLI arg > env (`HA_URL`/`HA_TOKEN`/`HA_TOKEN_FILE`) > token_file > inline token > URL из TOML.
5. Требования безопасности: права 0600 на конфиг с inline token и на token file, `O_NOFOLLOW` при открытии token file, redact токена во всех сообщениях об ошибках, запрет entity_id в payload интентов.

## Фазы переноса

### Phase 0 — каркас (готово)

- [x] Cargo-проект, бинарник `ha`, layout модулей 1:1 с Python
- [x] `errors.rs` — полный перенос типов ошибок и exit-кодов
- [x] `models.rs` — полный перенос `Entity`
- [x] `security.rs` — redact, `validate_entity_payload`, эвристики entity_id
- [x] `cli.rs` — clap-схема, скелет диспетчера
- [x] `docs/migration.md`

### Phase 1 — config + client (готово)

- [x] `config.rs`: разбор TOML, env, приоритеты, проверки прав (тесты `test_config.py` → `tests/config.rs`)
- [x] `security.rs`: `read_token_file` (O_NOFOLLOW/O_NONBLOCK через `std::os::unix::fs::OpenOptionsExt`), `require_secure_mode`
- [x] `client.rs`: `HttpTransport` на reqwest (rustls), таймауты, маппинг 401/403 → `AuthenticationError`, capture `Mcp-Session-Id`/`Mcp-Protocol-Version`, `initialize` → `tools_list` → `tools/call`
- [x] Тесты с мок-транспортом (аналог `test_client.py`, включая случай stale-session)

### Phase 2 — discovery (готово)

- [x] `discovery.rs`: `discover_tools` (basename по `__`), ambiguous-детект, кэш `${XDG_CACHE_HOME:-~/.cache}/ha-cli/tools.json` c 0600, TTL 300 c, refresh-on-stale (`is_stale_tool_result`)
- [x] Тесты: `test_discovery.py` → `tests/discovery.rs` (TTL, кэш-файл, дубликаты basename)

### Phase 3 — intents (готово)

- [x] `intents.rs`: `validate_intent` (готово), `execute`, `normalize_result`, `_normalize_arguments` (строка → массив, если schema ожидает array)
- [x] Интеграция с discovery: fallback `HassGetState` через live context
- [x] Тесты: `test_intents.py`, `test_security.py` → `tests/intents.rs`

### Phase 4 — context (готово)

- [x] `context.rs`: `get_raw_result`, `parse_live_context`, unwrap `{success, result}` с текстовым форматом `Live Context:` (парсер перенести дословно, включая кавычки-обёртки)
- [x] `build_context` (areas + aliases, сортировка), `compact_context`, `size_report`
- [x] `query_state` — casefold-сопоставление area/domain/name
- [x] Тесты: `test_context.py` → `tests/context.rs`

### Phase 5 — CLI-склейка и паритет (готово)

- [x] `cli.rs`: маршрутизация команд, парсинг payload (`serde_json`, значение обязано быть object), `--debug` с redact-трейсбеком
- [x] Сквозные тесты против мок-сервера (`test_cli.py`)
- [x] Сверка вывода с Python-версией на реальном HA (стенд claw.mrundead.org, aarch64: context/tools/intent + ошибочные сценарии — побайтово идентичны, включая exit-коды)

### Phase 6 — упаковка и деплой (готово)

- [x] CI: `cargo fmt --check && cargo clippy -- -D warnings && cargo test` (.github/workflows/ci.yml)
- [x] Release-сборка (`panic=abort`, LTO, strip) + артефакты x86_64/aarch64-linux-musl (.github/workflows/release.yml, теги `v*`; aarch64 — основная архитектура, собирается нативно на ubuntu-24.04-arm)
- [x] Скилл OpenClaw `ha-control` (skills/ha-control/SKILL.md + SKILL.toml) и install.sh с `--skills-root` (сборка, установка бинарника в ~/.local/bin, подстановка `{{HA_BIN}}`, откат при сбое) — заменяет deploy-nanobot.sh
- [ ] README репозитория: секция установки из релизов и скилла, пометить Python-версию как legacy

## Отличия, требующие внимания

- **String casefold**: в Rust нет встроенного Unicode casefold; для ASCII-имён областей достаточно `to_lowercase()`. Если паритет критичен — крейт `focaccia` (реализация Unicode casefold). Решение: начать с `to_lowercase()`, зафиксировать в тестах.
- **Динамическая типизация**: Python-код широко использует «может быть dict / list / str» — в Rust это `serde_json::Value` с ручными проверками (`as_str`, `as_object`). Не моделировать строгими struct'ами — формат от HA нестабилен.
- **Права файлов**: `fchmod 0600` для кэша/конфига — через `std::os::unix::fs::PermissionsExt`.
- **Trailing newline в JSON**: Python печатает `\n` после `json.dump` — сохранить (`println!` + `serde_json::to_string`).

## Критерии готовности

- `cargo test` зелёный, покрытие по смыслу не ниже pytest-набора
- Все контракты совместимости (раздел выше) выполняются
- `ha intent HassTurnOn '{"area":"кухня"}'` идентичен по выводу Python-версии
- Бинарник < 8 MiB, запускается на чистом хосте без Python

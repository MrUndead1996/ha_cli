# ha-cli

Лёгкий CLI-клиент для управления Home Assistant через сервер
[ha-mcp](https://github.com/homeassistant-ai/ha-mcp) и Home Assistant
intents. Написан на Rust, собирается в один бинарник.

## Установка / обновление

```bash
curl -fsSL https://raw.githubusercontent.com/MrUndead1996/ha_cli/main/install.sh | bash
```

Для установки релиза нужны `curl`, `tar` и `sha256sum`; `git` и `cargo`
нужны только для сборки из исходников. Добавь `~/.local/bin` в `PATH`
или выбери каталог через `--install-dir PATH`.

Отдельная установка скилла `ha-control`:

```bash
curl -fsSL https://raw.githubusercontent.com/MrUndead1996/ha_cli/main/install.sh | bash -s -- skill --skills-root PATH
```

Опции: `--install-dir PATH` — каталог для бинарника `ha` (укажи его
и при установке скилла, если он нестандартный);
`--skills-root PATH` — каталог скиллов. Сборка из исходников —
`--build-from-source` (требует подтверждения; `--force` пропускает
подтверждение).

## Возможности

- выполнение команд Home Assistant через intents;
- простой CLI-интерфейс для скриптов, агентов и автоматизаций;
- семантическое разрешение целей по `area` / `domain` / `name`;
  пользовательский `entity_id` отклоняется до любых сетевых запросов.

## Примеры

Команды CLI — `context`, `intent`, `tools`:

```bash
ha context --compact
ha intent HassTurnOn '{"area":"kitchen","domain":"light"}'
ha intent HassTurnOff '{"area":"kitchen","domain":"light"}'
ha intent HassLightSet '{"area":"kitchen","name":"Лампа","color_temp_kelvin":3000}'
ha intent HassGetState '{"area":"bathroom","domain":"binary_sensor"}'
ha tools --refresh
```

Селекторы — только семантические `area` / `domain` / `name`; сопоставление
точное (регистр не учитывается, нечёткого «похожего» выбора нет). Ноль
совпадений или неоднозначность — ошибка до вызова сервиса. Алиасы
сущностей работают как значения `name`; используй точные имена областей
из `ha context --compact`.

## Конфигурация

Файл `~/.config/ha-cli/config.toml` или переменные окружения. `.env`
автоматически не загружается.

- `mcp_url` (env `HA_MCP_URL`) — полный MCP endpoint ha-mcp. Путь URL
  является секретом и не выводится в ошибках и debug-выводе.
- `mcp_auth` (env `HA_MCP_AUTH`):
  - `none` (по умолчанию) — токен не отправляется, секретный URL
    авторизует запрос сам;
  - `ha_auth` — отправлять `HA_TOKEN` Bearer-заголовком; требует
    настроенный `mcp_url` и токен (`token_file`, файл 0600), иначе —
    ошибка конфигурации до сетевых запросов.
- Файл конфигурации с `mcp_url` или inline-токеном должен иметь права
  0600 (проверяются). Прочие TOML-ключи: `token`, `token_file`,
  `timeout`, `connect_timeout`.

Пример:

```toml
mcp_url = "https://home.example.local/api/webhook/<secret>"
mcp_auth = "ha_auth"   # или убери: секретный URL авторизует сам (none)
token_file = "~/.config/ha-cli/token"  # обязателен при ha_auth; файл 0600
# timeout = 60         # по умолчанию 60 c
# connect_timeout = 5
```

`timeout` — общий таймаут исполнения запроса. Таймаут после отправки
запроса не означает, что операция не выполнена — повторный вызов может
сработать дважды.

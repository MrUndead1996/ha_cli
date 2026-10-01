# Home Assistant CLI

Небольшой CLI-клиент для управления Home Assistant через MCP
(Assist или ha-mcp) и Home Assistant intents.

Проект написан на Rust и собирается в один бинарник без отдельного сервиса.

## Возможности

- выполнение команд Home Assistant через intents;
- работа через встроенный Assist MCP endpoint или сервер
  [ha-mcp](https://github.com/homeassistant-ai/ha-mcp) вместо прямой
  работы с отдельными entity/service API;
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
совпадений или неоднозначность — ошибка до вызова сервиса; цели не
угадываются. Алиасы сущностей работают как значения `name`, алиасы
областей при подключении к ha-mcp пока не поддерживаются — используй
точные имена областей из `ha context --compact`.

`ha intent` возвращает одну строку JSON с `ok`, `response_type`,
`speech` и опциональным `data`. При работе через ha-mcp текст `speech`
формируется заново и не обязан совпадать с прежним Assist-ответом
побайтово. `ha context --raw` при подключении к ha-mcp выводит
агрегированный ответ `ha_search` (новый формат), а не прежний текстовый
`GetLiveContext`.

## Конфигурация

Для Assist нужны `HA_URL` и Long-Lived Access Token (`HA_TOKEN`). Для
ha-mcp нужен полный `HA_MCP_URL`; токен администратора нужен при
`HA_MCP_AUTH=ha_auth`, но не при режиме `none`.

Параметры могут передаваться через конфигурацию или переменные окружения.
Файл `.env` CLI автоматически не загружает.

### Режим ha-mcp (`HA_MCP_URL`)

`HA_MCP_URL` (TOML `mcp_url`) задаёт полный MCP endpoint вместо прежнего
`HA_URL` + `/api/mcp/assist`. Путь URL является секретом (webhook
`/api/webhook/<secret>` или прямой `/private_<secret>`) и не выводится в
ошибках и debug-выводе.

Авторизация настраивается переменной `HA_MCP_AUTH` (TOML `mcp_auth`):

- `none` (по умолчанию) — токен **не** отправляется на `HA_MCP_URL`;
  секретный URL авторизует запрос сам по себе. Наличие `HA_TOKEN`
  в окружении само по себе не отправляет его на `HA_MCP_URL`.
- `ha_auth` — явное разрешение отправлять `HA_TOKEN` Bearer-заголовком
  на `HA_MCP_URL` (webhook-режим `ha_auth` с Long-Lived Access Token).
  Требует настроенный `HA_MCP_URL` и токен, иначе — ошибка конфигурации
  до сетевых запросов.

Прежний Assist endpoint (без `mcp_url`) продолжает работать с Bearer
`HA_TOKEN` как раньше. Приоритет `HA_MCP_AUTH`: env → TOML → `none`.
Конфигурационный файл с inline-токеном или `mcp_url` должен иметь права
0600 (проверяются; TOML-ключи: `url`, `mcp_url`, `mcp_auth`, `token`,
`token_file`, `timeout`, `connect_timeout`).

Пример конфигурации:

```toml
mcp_url = "https://home.example.local/api/webhook/<secret>"
mcp_auth = "ha_auth"   # или убери: секретный URL авторизует сам (none)
token_file = "~/.config/ha-cli/token"  # обязателен при ha_auth; файл 0600
# timeout = 60         # по умолчанию: 5 c для Assist, 60 c для ha-mcp
# connect_timeout = 5
```

Таймауты: `timeout` (TOML) — общий таймаут исполнения запроса,
`connect_timeout` — таймаут установки соединения. Явный `timeout`
всегда имеет приоритет. Таймаут после отправки запроса не означает, что
операция не выполнена — повторный вызов может сработать дважды.

### Откат на Assist / старый бинарник

CLI остаётся совместимым с прежним endpoint: чтобы откатиться, убери
`mcp_url` из конфигурации (и `HA_MCP_URL` из окружения) — CLI вернётся
к Assist `HA_URL` + `/api/mcp/assist` с Bearer `HA_TOKEN` и прежним
таймаутом 5 секунд. Файл кэша инструментов привязан к endpoint
(`~/.cache/ha-cli/tools-<endpoint>.json`, содержит только имена и схемы
инструментов, без секретов), поэтому кэши Assist и ha-mcp не мешают
друг другу. Полный откат — вернуть предыдущий бинарник и прежний
конфигурационный файл. После возврата на Assist `context --raw` и текст
`speech` снова используют формат Assist.

## Назначение

CLI предназначен как лёгкий интерфейс к Home Assistant для локальных AI-агентов и автоматизаций без необходимости использовать полный Home Assistant API напрямую.

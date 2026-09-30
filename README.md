# Home Assistant CLI

Небольшой CLI-клиент для управления Home Assistant через Assist и Home Assistant intents.

Проект написан на Rust и собирается в один бинарник без отдельного сервиса.

## Возможности

- выполнение команд Home Assistant через intents;
- использование Assist API вместо прямой работы с отдельными entity/service API;
- простой CLI-интерфейс для скриптов, агентов и автоматизаций;
- минимальный внешний контекст — логика разрешения команд остаётся на стороне Home Assistant.

## Пример

```bash
ha-cli turn-on --area kitchen --domain light
ha-cli turn-off --entity switch.light_kitchen
```

Команды преобразуются в соответствующие Home Assistant intents, например `HassTurnOn` и `HassTurnOff`.

## Конфигурация

Для подключения необходимы:

- URL Home Assistant;
- Long-Lived Access Token.

Параметры могут передаваться через конфигурацию или переменные окружения.

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
0600.

## Назначение

CLI предназначен как лёгкий интерфейс к Home Assistant для локальных AI-агентов и автоматизаций без необходимости использовать полный Home Assistant API напрямую.

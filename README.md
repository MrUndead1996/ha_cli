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

## Назначение

CLI предназначен как лёгкий интерфейс к Home Assistant для локальных AI-агентов и автоматизаций без необходимости использовать полный Home Assistant API напрямую.

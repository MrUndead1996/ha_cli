# Миграция CLI с Assist MCP на ha-mcp

## Цель и границы

Перевести `ha` с встроенного Home Assistant MCP (`/api/mcp/assist`) на
[ha-mcp](https://github.com/homeassistant-ai/ha-mcp), сохранив лёгкий CLI для
управления домом по `area` / `domain` / `name`. Добавить возможность передавать
параметры сервисов, в частности параметры цветовой температуры для `light`.
Список доступных CLI сущностей приходит от настроенного сервера ha-mcp уже
отфильтрованным. CLI использует именно этот список для разрешения целей и не
пытается воспроизвести фильтрацию Assist или расширить область поиска.

**Неизменное правило:** пользовательский payload не принимает `entity_id` ни
как ключ, ни как значение вида `light.kitchen`. Идентификатор, полученный от
ha-mcp при поиске, применяется только внутри CLI после однозначного разрешения
семантической цели. Не вводить публичный `tools/call` или произвольный вызов
сервисов, позволяющий обойти это правило.

## Текущее состояние

- `src/client.rs`: HTTP JSON-RPC с фиксированным `/api/mcp/assist`, Bearer
  `HA_TOKEN`, JSON-ответами и таймаутом 5 секунд.
- `src/context.rs`: `GetLiveContext`, парсер его текстового формата, вывод
  `context` / `context --compact`; fallback чтения состояния.
- `src/intents.rs`: белый список из пяти `Hass*`, нормализация ответа Assist
  (`response_type`, `speech`, `data`).
- `src/discovery.rs`: кэш схем инструментов на 300 секунд, общий для всех
  подключений; `tools --json` выводит схемы исходного сервера.
- `src/security.rs`: рекурсивный запрет прямой адресации по `entity_id`.
- `skills/ha-control/SKILL.md`: примеры, разрешённые команды и формат ответов
  завязаны на Assist.

Существующий `docs/migration.md` описывает завершённый перенос Python → Rust;
этот документ относится только к смене MCP-сервера.

## Целевой контракт CLI

1. `ha context [--raw|--compact]` показывает **весь разрешённый сервером**
   список сущностей, не только первую страницу. Сохраняем удобный вывод
   `areas → domain → names`; `--raw` документируем как новый ответ ha-mcp,
   поскольку старый `GetLiveContext` исчезнет.
2. `ha intent HassTurnOn|HassTurnOff|HassLightSet|HassSetPosition|HassGetState
   '{...}'` на первом этапе остаётся совместимой оболочкой для скилла.
   Семантические селекторы — `area`, `domain`, `name`. Результат нормализуется
   до текущего JSON-конверта `ok`, `response_type`, `speech`, опционально
   `data`, но содержимое `speech` не обязано совпадать с Assist побайтово.
3. Для параметров света расширить разрешённый payload `HassLightSet`:
   например, `{"area":"кухня","name":"Лампа","color_temp_kelvin":3000}`.
   Проверить фактическое имя и диапазон поля в `ha_list_services` для
   `light.turn_on` на целевой установке; не подменять `color_temp` и
   `color_temp_kelvin` автоматически без проверенного соответствия.
   Допустимые параметры сервисов задавать явным списком по интенту/домену;
   не передавать произвольные ключи payload в `data`.
4. `ha tools` остаётся диагностической командой просмотра доступных MCP
   инструментов; наличие инструмента не означает разрешения вызывать его
   через CLI.

## Этапы реализации

### 1. Развёртывание и контрольный контракт

- Выбрать один HTTP-вариант ha-mcp: сервер в custom component, app (add-on)
  либо отдельный HTTP-сервер. Для in-process варианта сверить поддерживаемую
  версию Home Assistant с документацией выбранного релиза. Записать версию
  ha-mcp и фактические `tools/list`, схемы `ha_search`, `ha_get_state`,
  `ha_call_service`, `ha_list_services` и примеры их ответов.
- Подтвердить на стенде, что поиск и перечисление возвращают именно
  согласованный отфильтрованный набор сущностей. Не считать автоматически,
  что поведение ha-mcp повторяет Assist: по умолчанию сервер предоставляет
  более широкий доступ к Home Assistant. Если фильтрация не подтверждена,
  не переключать CLI на этот endpoint.
- Проверить для одного света схему `light.turn_on` через `ha_list_services`
  (`detail_level="full"`) и прочитать состояние/атрибуты через
  `ha_get_state`. Вызов с параметром цветовой температуры тестировать только
  на согласованной тестовой сущности.

### 2. Подключение и протокол (`src/config.rs`, `src/client.rs`)

- Добавить настройку полного MCP endpoint URL, отдельно от `HA_URL`.
  Убрать безусловное добавление `/api/mcp/assist`. Учесть webhook URL
  `/api/webhook/<secret>` и прямой адрес `/private_<secret>`: путь URL является
  секретом, его следует регистрировать для редактирования в сообщениях об
  ошибках; не выводить его в debug и диагностическом JSON.
- Поддержать способ авторизации выбранного endpoint: секретный URL без
  Bearer-токена либо явно настроенный Bearer-токен, если того требует
  развёртывание. Не отправлять `HA_TOKEN` на другой адрес по умолчанию.
  Сохранить требования 0600 к файлам с секретами и redaction.
- Проверить на реальном сервере и покрыть тестами `initialize`, session ID,
  версию протокола и ответы `tools/list` / `tools/call`. Обрабатывать и JSON,
  и `text/event-stream` (сопоставлять JSON-RPC `id`), а не пытаться
  десериализовать SSE как один JSON. Разделить таймаут подключения и таймаут
  исполнения: сервисы ha-mcp могут ожидать подтверждения изменения состояния.
- Различать ошибку HTTP/авторизации, JSON-RPC-ошибку, `isError` инструмента
  и `{ "success": false, ... }` в результате; секреты не должны попадать в
  stderr даже при ответе сервера с отражённым URL.

Реализовано (этап 2.4): HTTP 401/403 различимы как `authentication_error`
(exit 4), прочие HTTP >=400 и JSON-RPC `error` — `ha_api_error` (exit 6,
сообщение сервера проходит `Secrets.redact`); `tools/call` с `isError=true`
(текст content либо структурированное сообщение ToolError
`structuredContent.error`/`.message`) и `{ "success": false, ... }` внутри
`structuredContent` или JSON в тексте content — `intent_failed` (exit 7),
такие результаты никогда не выдаются как успешные. Те же проверки применены
в `parse_live_context` (`context_error`, exit 10) до требования наличия
content; legacy-конверт GetLiveContext `{success, result}` при `success:true`
распаковывается как раньше. Общие хелперы вынесены в `src/tool_result.rs`. Финальный JSON ошибки
на stderr и debug-трассировка проходят `Secrets.redact`: отражённый webhook
URL, часть path, секретный сегмент и токен редактируются. Повторный вызов
инструмента после отправки действия не выполняется (ошибки чтения после
отправки помечаются «may still have been performed»).

Реализовано (этап 2.2): выбранная схема авторизации — `HA_MCP_AUTH=ha_auth`
(TOML `mcp_auth="ha_auth"`) как явный opt-in на отправку `HA_TOKEN` Bearer
на `HA_MCP_URL`; по умолчанию `none` — секретный URL авторизуется сам,
`HA_TOKEN` из окружения на `HA_MCP_URL` не отправляется. Assist endpoint
без `mcp_url` сохраняет прежнее поведение. Требования 0600 к конфигу с
секретами и redaction URL/токена сохранены.

### 3. Семантическое разрешение (`src/context.rs`, `src/models.rs`)

Реализовано (этап 3.3): `HassGetState` на ha-mcp (`src/intents.rs`,
`execute_hamcp_get_state`) разрешает цели строго по полному каталогу
`ha_search` через `resolver::prepare_action` (семантические селекторы,
bound, fail-closed на malformed-записях — всё до любого чтения состояния),
затем читает состояние `ha_get_state` по внутренним ID — по одному ID за
вызов (`fields=["state","attributes"]`; форма ответа
`{data:{state,attributes}, metadata}` подтверждена на живом сервере).
Поддержан возможный bulk-wrapper (`states`/`errors`/`count`/`error_count`):
непустые `errors`/`error_count` распространяются как ошибка, `state` берётся
только из единственного элемента `states`. Состояние всегда из живого
`ha_get_state`, а не из устаревшего поля `state` каталога; `isError`,
`success:false` и отсутствие/нестроковость `state` — fail-closed ошибки
(`intent_failed`), а не «unknown». Ответ собирается в прежнем конверте
`ok`/`response_type:"query_answer"`/`speech`/`data.states` с семантическими
именами area/domain/name; внутренние ID наружу не отражаются — scrub
по всем подготовленным целям применяется к ЛЮБОЙ ошибке после разрешения
(ошибки JSON-RPC/транспорта от `tools_call`, stale-refresh и discovery
включительно), так как текст сервера может отражать переданные ID.
Stale-refresh кэша схем разрешён только для этого read-only вызова;
повторные вызовы действий после отправки по-прежнему не выполняются.
Assist-путь без `mcp_url` (fallback `query_state`) не изменён.

Реализовано (этап 3.2): `src/resolver.rs` — строгое разрешение целей
действий по `area` / `domain` / `name` из отфильтрованного каталога
`ha_search` (через `get_live_context`, собираемого заново на каждое
действие — список не кэшируется между действиями). Сопоставление точное
(trim + lowercase, без нечёткого выбора «первого похожего»); записи
каталога с одинаковым `entity_id` дедуплицируются. Точечный `name`:
ноль или несколько совпадений — ошибка (`intent_failed` /
`ambiguous_tool`) ДО вызова инструмента действия. Массовое действие —
только по явно указанной area/domain (или обеим); набор разрешается
полностью до выполнения, число целей зафиксировано (`Prepared.bound`),
превышение `MAX_ACTION_TARGETS` (64) — ошибка. Алиасы сущности
(`ha_search.aliases`, `Entity.entity_aliases`) используются только как
точные имена для `name` и никогда как область. `prepare_action` сама
рекурсивно проверяет `validate_entity_payload` на входе (до любой сети),
независимо от вызывающей стороны; каждая совпавшая цель обязана иметь
валидный непустой внутренний `entity_id` из `ha_search` — запись
каталога без ID (malformed) даёт fail-closed ошибку, а не цель без
идентификатора. `validate_entity_payload`
по-прежнему отклоняет пользовательский `entity_id` до сети; в подготовке
ha-mcp вызовов (`intents::execute` при `mcp_url`) после успешного разрешения
действие исполняется через `ha_call_service` — путь исполнения добавлен на
этапе 4.2 (см. раздел «Действия и политика»).
Assist-путь без `mcp_url` не изменён.

Реализовано (этап 3.1): `GetLiveContext` заменён на `ha_search` только при
настроенном `mcp_url` (ha-mcp 8.6.0); Assist-путь без `mcp_url` и его тесты
сохранены. Полный каталог строится как: `ha_get_overview(fields=["domain_stats"])`
(только имена доменов, сущности из обзора не берутся) → постраничный
`ha_search(domain_filter=…, limit<=200, offset…, result_fields…)` для каждого
домена; пагинация следует `entity_has_more` / `entity_next_offset`, ID
дедуплицируются, сумма `entity_total_matches` по доменам сверяется с числом
собранных сущностей.
При `partial=true`, непустом `errors`, `entity_has_more=true` без валидного
`entity_next_offset` или несовпадении числа собранных сущностей с
`entity_total_matches` выдаётся `context_error` (exit 10), неполный каталог
не выдаётся никогда. `Entity` (`src/models.rs`) принимает оба формата:
ha-mcp (`friendly_name`, `entity_id`, массив `aliases` алиасов СУЩНОСТИ) и
прежний Assist
(`name`, область с алиасами ОБЛАСТИ через запятую). Алиасы сущности хранятся
в `Entity.entity_aliases` отдельно и не попадают в `area_aliases` и
сопоставление области; `area_aliases` для ha-mcp на этом этапе пусты — если
потребуются алиасы областей, их берут отдельным вызовом `ha_list_floors_areas`
только после анализа его схемы и источника данных. `context` / `context --compact`
работают на агрегированном ответе; `context --raw` с `mcp_url` выводит
агрегированный ответ `ha_search` (новый формат вместо ответа
`GetLiveContext`); секреты (webhook URL, токен) в вывод и логи не попадают.

- Заменить `GetLiveContext` на `ha_search` с фильтрами и явной обработкой
  `limit` / `offset` / `has_more`. Для построения контекста получить все
  страницы; при `partial`, ошибке или недоступной странице не выдавать
  неполный каталог как полный. Проверить соответствие полей ответа модели
  `Entity` и наличие area/aliases; при необходимости использовать
  `ha_list_floors_areas` для точного сопоставления имён и алиасов областей.
- При действии искать сущность в отфильтрованном каталоге по точным
  семантическим селекторам (регистр можно игнорировать), без нечёткого выбора
  «первого похожего». Ноль совпадений и неоднозначное совпадение — ошибки.
  Массовое действие по области/домену выполнять только по явно указанной
  области/домену; зафиксировать ограничение числа целей и сначала разрешить
  весь набор, затем выполнять действие. Не расширять область поиска при
  неудаче. Повторно проверять набор для каждого действия: кэш схем
  инструментов не является кэшем доступных сущностей.
- Для `HassGetState` использовать `ha_get_state` по внутренне разрешённым ID
  и собирать прежний ответ для семантических имён. Для `context --compact`
  сохранить структуру вывода; перестать разбирать текст `Live Context:`.

### 4. Действия и политика (`src/intents.rs`, `src/security.rs`)

Реализовано (этап 4.1): строгая интент-специфичная валидация payload
`security::validate_intent_payload` на ha-mcp-пути (`mcp_url` задан) —
ДО любых сетевых вызовов, включая `resolver::prepare_action` и
`ha_get_state`; рекурсивный `validate_entity_payload` остаётся и в самой
`prepare_action` (защита от обхода). Проверяются: неизвестные ключи по
интент-специфичному allowlist; типы и диапазоны значений; непустые
строковые селекторы `area` / `domain` / `name` (не-строковый селектор —
ошибка, а не молчаливое игнорирование); допустимые сочетания параметров.
Allowlist: `HassTurnOn` / `HassTurnOff` / `HassGetState` — только
`area` / `domain` / `name`; `HassLightSet` — плюс `brightness`
(целое 0..100 как целочисленный JSON-литерал; дробные литералы вида
`50.0` отклоняются как другой тип — семантика скилла; на ha-mcp этап 4.2
отобразится в подтверждённое поле `brightness_pct` сервиса
`light.turn_on`),
`color_temp_kelvin` (целое 1000..12000, подтверждено живым
`ha_list_services`), `transition` (конечное число 0..300 секунд,
дробное допустимо), `rgb_color`
(3 целых 0..255), `effect` (непустая строка); `color_temp_kelvin` и
`rgb_color` взаимно исключающие (разные цветовые режимы). `HassSetPosition`
— плюс обязательный `position` (целое 0..100 как целочисленный
JSON-литерал, дробные отклоняются). Если `domain` указан
селектором, `HassLightSet` требует домен `light`, а `HassSetPosition` —
`cover`; проверка выполняется до сети, по селектору. Assist-путь без
`mcp_url` не изменён: только прежний `validate_entity_payload`, payload
передаётся инструменту как раньше.

- Сохранить рекурсивную проверку `validate_entity_payload` **до любых**
  сетевых действий. Проверять неизвестные ключи, типы и допустимые сочетания
  параметров перед вызовом инструмента.
- Отобразить включение/выключение на `ha_call_service` для допустимого
  домена; `HassLightSet` — на `light.turn_on` с прошедшими валидацию
  параметрами; `HassSetPosition` — на соответствующий сервис домена после
  проверки каталога сервисов. Передавать `entity_id` серверу только из
  результата разрешения. Для нескольких целей учитывать семантику ожидания
  ha-mcp: не полагаться на `wait=true` для списка ID через запятую;
  выбрать последовательные вызовы с поштучным результатом либо явно
  документированный массовый инструмент.
- Сохранить запрет на вызов административных и конфигурационных инструментов
  через `intent`. Ошибку ожидания/потери связи после отправки действия не
  трактовать как доказательство, что действие не выполнено: автоматический
  повтор вызова может выполнить его дважды.

Реализовано (этап 4.3, подпункт 3 — аудит и закрытие пробелов). Запрет
административных/конфигурационных инструментов через `intent` подтверждён
аудитом маршрута `CLI -> intents::execute -> resolver::prepare_action ->
tools_call`: имена инструментов — константы кода (`HA_GET_STATE_TOOL`,
`HA_CALL_SERVICE_TOOL`), домен и сервис выбирает только CLI по интенту
(`TURN_SERVICE_DOMAINS`, фиксированные `light.turn_on` /
`cover.set_cover_position`), `data` собирается только из полей, прошедших
`validate_intent_payload` (allowlist ключей без `data` / `service` / `tool` /
`wait` и с рекурсивным запретом `entity_id`), CLI-команда `tools` — только
диагностика, raw `tools/call` через CLI отсутствует.

Неопределённый исход записи: на ha-mcp-пути вызов `ha_call_service` никогда
не повторяется автоматически. Проверены обе формы stale tool name: MCP
`isError` с текстом «Tool … not found» и JSON-RPC `error` (например,
`-32602 unknown tool`) — обе завершаются ошибкой `intent_failed` без
повторного вызова и с пометкой «may still have been performed».
Формулировка ошибки транспорта нейтральна к факту отправки: при
DNS-fail/отказе соединения запрос мог не покинуть клиента, а при JSON-RPC
error — дойти, но не выполниться; клиент не может это различить, поэтому
текст не утверждает «sent» (только «whether the request reached the server
is unknown … not repeated automatically»). Discovery сбоит до первого
вызова записи (поиск инструмента выполняется один раз до цикла целей), а
частичный успех по целям не откатывается и не дозаказывается; сообщение
называет число выполненных целей и вычищает внутренние ID. Read-only
stale-refresh разрешён только `HassGetState` / `context`. Assist-путь без
`mcp_url` не изменён.

Реализовано (этап 4.2): действия `HassTurnOn` / `HassTurnOff` /
`HassLightSet` / `HassSetPosition` на ha-mcp (`mcp_url` задан) исполняются
через `ha_call_service` (`src/intents.rs`, `execute_hamcp_action`). Полный
набор целей разрешается ДО первого вызова сервиса (`resolver::prepare_action`:
рекурсивный запрет пользовательского `entity_id` до любой сети, внутренние ID
только из полного каталога `ha_search`, fail-closed на malformed-записях,
`ambiguous_tool`/ноль совпадений/превышение bound — до вызова сервиса). Домен
каждой цели проверяется по интенту после разрешения и до сети: `HassLightSet`
— только `light`, `HassSetPosition` — только `cover` (даже если селектор
`domain` в payload отсутствовал — массовый выбор по `area` не заденет чужой
домен); `HassTurnOn` / `HassTurnOff` — явный allowlist
(`intents::TURN_SERVICE_DOMAINS`: light, switch, fan, cover, media_player,
humidifier, water_heater, siren, vacuum); домены вне списка не вызываются.
Произвольные сервисы, админские инструменты и raw calls через интент
невозможны: домен и сервис выбирает только CLI по интенту.

Отображение на сервисы: `HassLightSet` → `light.turn_on` с
валидированными полями и только ими — `brightness` (percent) →
подтверждённое поле `brightness_pct`, `color_temp_kelvin` → одноимённое
поле, `transition` / `rgb_color` / `effect` как есть; `HassSetPosition` →
`cover.set_cover_position` c `data.position` (контракт инструмента
`ha_call_service(domain, service, entity_id, data, wait)` и поле `position`
перепроверены по актуальной документации ha-mcp/HA; реальный сервер для
проверки не использовался). `HassTurnOn` / `HassTurnOff` →
`<домен цели>.turn_on` / `.turn_off` без data.

Multi-target: набор фиксируется до выполнения, затем ровно один
`ha_call_service` на цель с одиночным `entity_id` — comma-separated список с
`wait=true` не используется никогда (параметр `wait` ha-mcp подтверждает
изменение только одиночного `entity_id`; comma-separated падает в legacy-путь
опроса с 10-секундным таймаутом). Результат каждого вызова нормализуется:
`isError`, `{success:false}`, `partial:true`, непустые `errors`/`warnings` —
ошибки. Вызов записи после отправки не повторяется: timeout, stale tool name
и ошибки ответа не доказывают, что сервис не выполнился. При сбое на M-й
цели возвращается ошибка `intent_failed` с безопасной суммарной диагностикой
(«выполнено для N из M целей», выполненные вызовы не откатываются, ID
вычищаются через `scrub_internal_ids`, успешный JSON ID не содержит).
Assist-путь без `mcp_url`, `HassGetState` (read-only) и exit/JSON contract
не изменены; тесты — только mock, live e2e не выполнялся.

Группы и агрегаты (ревью 4.2): проекция `ha_search` расширена полем
`is_group` (`context::HA_SEARCH_RESULT_FIELDS`, opt-in enrichment по
документации ha-mcp), флаг хранится в `Entity.is_group`. Любая цель
подготовленного действия с `is_group=true` или в домене `group`
отклоняется ДО первого `ha_call_service` явной ошибкой (`intent_failed`,
имя цели, без внутреннего ID) — группы никогда не отфильтровываются
молча; набор не урезается, действие отменяется целиком. Read-only
`HassGetState` и `context` эту проверку не применяют (семантика
сохранена). Обработка отсутствия/нечитаемости флага: поле отсутствует
(прежний Assist, либо установка ha-mcp без разметки агрегатов) —
считается `false`, Assist-путь не меняется; поле присутствует НЕ булевым
значением — fail-closed как `true` (нечитаемую разметку нельзя считать
гарантией листа). Ограничение: если ha-mcp-установка вообще не размечает
какой-то агрегат (поле отсутствует у групповой сущности), CLI не может
отличить её от листа и выполнит действие — обнаружение таких агрегатов
невозможно без `is_group`; разметку подтверждать на стенде (этап 5).

### 5. Кэш, тесты, документация и переключение

- Разделить кэш `src/discovery.rs` по endpoint/типу сервера; старый Assist
  кэш не должен подставить `Hass*` в сессию ha-mcp. Сохранить ручное
  `tools --refresh`; учесть отключённые инструменты и режим tool search
  (если нужен прямой вызов перечисленных выше инструментов — держать их
  доступными и проверить через `tools/list`).
- Обновить моки и интеграционные тесты: URL и авторизация, JSON/SSE,
  пагинация и `partial`, ноль/одно/несколько совпадений, запрет ID в любой
  вложенности, разрешённый параметр цветовой температуры, ошибки сервиса,
  redaction секретного URL и отсутствие повторного выполнения действия.
  Запустить `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.

Реализовано (этап 5, подпункт 2 — аудит тестов): юнит/mock-покрытие этапов
2–4 сохранено без дублирования (URL/auth/SSE — `src/client.rs`, пагинация
и partial/errors fail-closed — `tests/context.rs`, запрет entity_id в любой
вложенности до сети — `src/intents.rs` NoNetwork и `tests/intents.rs`,
`color_temp_kelvin` → `data.color_temp_kelvin`, scrub и no-retry —
`src/intents.rs`, изоляция кэша по endpoint — `tests/discovery.rs`,
redaction — `tests/cli.rs`/`tests/client.rs`). Добавлен недостающий
межмодульный сценарий `tests/hamcp_dispatch.rs` — от разбора `Cli` через
`dispatch` до RecordingTransport на ha-mcp: URL как есть (без суффикса
Assist) и Bearer `ha_auth`/отсутствие Authorization по умолчанию на каждом
запросе, SSE `initialize`/`tools/list` сквозь весь поток, пагинация
каталога по `entity_next_offset` (2 страницы, сервер режет страницу до 2),
`HassLightSet color_temp_kelvin` → единственный `ha_call_service` с
`data.color_temp_kelvin` и только валидированными полями, read-only
`HassGetState` — успешный вывод `query_answer` без `ha_call_service` и без
внутренних ID, вложенный `entity_id` — отклонён с нулём сетевых вызовов,
JSON-RPC error на записи — ровно один вызов без повтора и «may still have
been performed» с вычищенными ID. Дефектов кода аудит не выявил.
- Обновить `skills/ha-control/SKILL.md`, `README.md`, примеры настройки
  и описание того, что `--raw`/`speech` меняют источник данных. На стенде
  проверить `context`, состояние, включение/выключение и параметры света;
  затем переключить endpoint в конфигурации CLI. Сохранять возможность
  отката на прежний бинарник и конфигурацию до завершения проверки.

## Критерии приёмки

- CLI видит только согласованный отфильтрованный набор; запросы по неизвестной
  или неоднозначной цели не вызывают сервис.
- Пользовательский `entity_id` (в том числе внутри вложенного JSON) отклонён
  до сети; во всех `tools/call` ID появляется лишь после разрешения CLI.
- Задание цветовой температуры для выбранного света проходит через
  `light.turn_on` с параметром, подтверждённым каталогом сервисов, и
  результат/ошибка возвращаются одной строкой JSON по контракту CLI.
- Контекст не теряет сущности из-за пагинации и явно сообщает о неполноте;
  ошибки и отладочный вывод не раскрывают URL с секретом или токен.

## Результаты этапа 1: проверка действующего сервера (2026-09-30)

Проверено запросами к запущенному HACS in-process серверу на Home Assistant
2026.9.4 (версия HA сообщена владельцем). Подключение через webhook в режиме
`ha_auth` с Long-Lived Access Token администратора: `/api/` принимает токен,
HA `auth/current_user` возвращает `is_admin=true`. Сам URL и токен в репозиторий
не записывались.

- `initialize`: HTTP 200, `Content-Type: text/event-stream`, MCP
  `protocolVersion=2025-03-26`, `serverInfo={name: "ha-mcp", version: "8.6.0"}`.
  `notifications/initialized`: HTTP 202. Сервер не прислал `Mcp-Session-Id`
  в проверенном обмене; поддержка заголовка в будущем всё равно нужна.
- `tools/list`: HTTP 200 с SSE, 77 инструментов; доступны `ha_search`,
  `ha_get_state`, `ha_call_service`, `ha_list_services`. У `ha_search`
  параметры `query`, `domain_filter`, `area_filter`, `limit`, `offset`,
  `include_hidden`, `result_fields`, `fields`; `ha_get_state` требует
  `entity_id`; у `ha_call_service` есть `domain`, `service`, `entity_id`,
  `data`, `wait`; у `ha_list_services` — `domain`, `detail_level`, `limit`,
  `offset`. Версию 8.6.0 подтвердил также `ha_get_overview` с проекцией
  `fields=["ha_mcp_update"]`.
- `ha_search(domain_filter="light", limit=1)` вернул одну сущность,
  `entity_total_matches=1`, `partial=false`, `entity_has_more=false`.
  Ответ содержит `entities`, `entity_total_matches`, `partial`, `errors`,
  `warnings`, `entity_has_more`, `entity_next_offset`, `offset`, `limit`.
  Для полного списка учитывать **именно** `entity_has_more` /
  `entity_next_offset`, а также `partial` и `errors`; `limit=1` здесь не
  доказывает поведение на нескольких страницах.
- `ha_get_state` по ID, полученному из `ha_search`, успешно вернул
  `data={state, attributes}` и `metadata`. В атрибутах проверенной лампы
  есть `supported_color_modes`, `brightness`, `color_temp_kelvin`.
- `ha_list_services(domain="light", detail_level="full")` подтвердил
  сервис `light.turn_on` с полем `color_temp_kelvin` (необязательное,
  selector `color_temp`). Другие поля этого сервиса включают `transition`,
  `rgb_color`, `brightness_pct`, `brightness_step_pct`, `effect`.

Дополнительная проверка области `bathroom`: `ha_search(area_filter="bathroom")`
вернул три сущности, `partial=false`, ошибок нет. Владелец подтвердил, что
неэкспонированные сущности этой области отсутствуют в результате. Это
подтверждение состава владельцем, а не отдельный поиск по имени скрытой
сущности. Для `bathroom` проверена пагинация с `limit=1`: три страницы по
одной сущности, `entity_next_offset` последовательно `1`, `2`, затем `null`,
`entity_has_more` — `true`, `true`, `false`; сумма равна
`entity_total_matches=3`. Этап 1 завершён без вызовов сервисов управления.

## Источники для сверки при реализации

- [ha-mcp: варианты установки и перечень инструментов](https://github.com/homeassistant-ai/ha-mcp)
- [In-process сервер: URL и режимы авторизации](https://github.com/homeassistant-ai/ha-mcp/blob/master/docs/in-process-server.md)
- [Каталог инструментов: `ha_search`, `ha_get_state`, `ha_call_service`, `ha_list_services`](https://homeassistant-ai.github.io/ha-mcp/tools/)
